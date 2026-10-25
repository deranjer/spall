//! Server-owned world items (ENG-257).
//!
//! Small things lying in the world that a player can pick up: a stable id, a
//! game-defined kind and a pose ([`WorldItem`]). They own no voxels and have no
//! physics. Only the owning thread creates or removes them, between ticks, and
//! every change is queued here until the host drains it for replication.
//!
//! A fire (ENG-266) is a world item with fuel ([`WorldItem::fire_fuel_s`]).
//! It burns down each tick, goes out at zero and stays as an unlit pit that
//! refuelling relights. Lit fires warm whoever is near ([`heat_at`]).

use std::collections::HashMap;

use spall_protocol::{WIRE_SCHEMA_VERSION, WorldItem, WorldItemUpdate};
use spall_voxel::Volume;

/// How far from a player's eye an item can be picked up, metres. The server
/// measures it from its own player; clients use it only for the prompt.
pub const ITEM_PICKUP_REACH_M: f64 = 3.0;
/// Thin snow deeper than this over an item buries it: it cannot be picked up
/// until someone shovels the snow off.
pub const ITEM_BURIED_SNOW_M: f64 = 0.15;

// Fire tuning (ENG-266). How much fuel an item adds is the game's call; these
// are the engine's side: where a fire may go, how it burns, how it warms.

/// How far from a player's eye a fire can be set down, metres.
pub const FIRE_PLACE_REACH_M: f64 = 3.0;
/// When the aim meets no ground in reach, the fire goes this far in front of
/// the player instead, metres.
pub const FIRE_FRONT_M: f64 = 1.2;
/// Half the width of the open-air box a fire needs on the ground, and its
/// height, metres: the pit's stones must not cut into a wall.
const FIRE_FOOTPRINT_HALF_M: f64 = 0.3;
const FIRE_CLEARANCE_M: f64 = 0.75;
/// Warming beside a lit fire, in degrees Celsius of felt temperature, falling
/// to nothing at [`FIRE_HEAT_RADIUS_M`] from its centre.
pub const FIRE_HEAT_C: f32 = 30.0;
pub const FIRE_HEAT_RADIUS_M: f32 = 4.0;
/// Height of a fire's centre (its heat and light) above its resting point.
pub const FIRE_CENTER_M: f64 = 0.3;
/// Fuel burnt per second by a fire in the open during a blizzard; sheltered
/// fires and every other weather burn 1 s per second.
pub const BLIZZARD_BURN_RATE: f32 = 2.0;
/// A fire with solid cells within this height above it is sheltered.
const FIRE_SHELTER_M: f64 = 8.0;

/// Why an item could not be created or restored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum WorldItemError {
    #[error("world item kind must be nonzero and its pose finite")]
    Invalid,
    #[error("world item id {0} is already used or beyond the id counter")]
    BadId(u64),
}

/// The live items plus the changes not yet replicated.
#[derive(Debug)]
pub(crate) struct WorldItems {
    items: HashMap<u64, WorldItem>,
    /// The id the next spawn gets. Ids are never reused within a world.
    next_id: u64,
    /// Spawned and changed items not yet announced, each id once.
    spawned: Vec<WorldItem>,
    removed: Vec<u64>,
    /// Items with ids below this have been announced (ids only grow).
    announced_below: u64,
}

impl Default for WorldItems {
    fn default() -> Self {
        Self {
            items: HashMap::new(),
            next_id: 1,
            spawned: Vec::new(),
            removed: Vec::new(),
            announced_below: 1,
        }
    }
}

impl WorldItems {
    pub(crate) fn spawn(
        &mut self,
        kind: u16,
        translation_m: [f64; 3],
        yaw_rad: f32,
        fire_fuel_s: Option<f32>,
    ) -> Result<WorldItem, WorldItemError> {
        let item = WorldItem {
            item_id: self.next_id,
            kind,
            translation_m,
            yaw_rad,
            fire_fuel_s,
        };
        if !item.is_valid() {
            return Err(WorldItemError::Invalid);
        }
        self.next_id += 1;
        self.items.insert(item.item_id, item);
        self.spawned.push(item);
        Ok(item)
    }

    pub(crate) fn remove(&mut self, item_id: u64) -> Option<WorldItem> {
        let item = self.items.remove(&item_id)?;
        if let Some(i) = self.spawned.iter().position(|s| s.item_id == item_id) {
            self.spawned.swap_remove(i);
        }
        // A spawn not yet announced needs no announced removal.
        if item_id < self.announced_below {
            self.removed.push(item_id);
        }
        Some(item)
    }

    /// Adds `fuel_s` seconds to a fire, relighting it, and queues the change.
    /// `None` for an unknown id, an item that is not a fire, or fuel that is
    /// not a positive finite amount.
    pub(crate) fn add_fuel(&mut self, item_id: u64, fuel_s: f32) -> Option<WorldItem> {
        if !(fuel_s.is_finite() && fuel_s > 0.0) {
            return None;
        }
        let item = self.items.get_mut(&item_id)?;
        let fuel = item.fire_fuel_s.as_mut()?;
        *fuel += fuel_s;
        let item = *item;
        self.announce(item);
        Some(item)
    }

    /// Burns `dt_s` seconds of every lit fire at `rate(fire)` fuel seconds per
    /// second. A fire that runs out goes out (fuel 0) and is announced; one
    /// still burning is not, so a fire costs the wire nothing while it burns.
    pub(crate) fn burn(&mut self, dt_s: f32, rate: impl Fn(&WorldItem) -> f32) {
        let mut out = Vec::new();
        for item in self.items.values_mut() {
            if !item.is_lit() {
                continue;
            }
            let burnt = dt_s * rate(item).max(0.0);
            let fuel = item.fire_fuel_s.as_mut().expect("lit fires have fuel");
            *fuel -= burnt;
            if *fuel <= 0.0 || !fuel.is_finite() {
                *fuel = 0.0;
                out.push(*item);
            }
        }
        for item in out {
            self.announce(item);
        }
    }

    /// Queues `item`'s new state, replacing an unannounced earlier one.
    fn announce(&mut self, item: WorldItem) {
        match self.spawned.iter_mut().find(|s| s.item_id == item.item_id) {
            Some(queued) => *queued = item,
            None => self.spawned.push(item),
        }
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &WorldItem> {
        self.items.values()
    }

    pub(crate) fn next_id(&self) -> u64 {
        self.next_id
    }

    /// Replaces every item (save recovery or a world reset). Nothing is
    /// queued: a client receives the new set with its next baseline.
    pub(crate) fn restore(
        &mut self,
        next_id: u64,
        items: Vec<WorldItem>,
    ) -> Result<(), WorldItemError> {
        let mut by_id = HashMap::with_capacity(items.len());
        for item in items {
            if !item.is_valid() {
                return Err(WorldItemError::Invalid);
            }
            if item.item_id >= next_id || by_id.insert(item.item_id, item).is_some() {
                return Err(WorldItemError::BadId(item.item_id));
            }
        }
        *self = Self {
            items: by_id,
            next_id: next_id.max(1),
            spawned: Vec::new(),
            removed: Vec::new(),
            announced_below: next_id.max(1),
        };
        Ok(())
    }

    pub(crate) fn take_changes(&mut self) -> Vec<WorldItemUpdate> {
        self.announced_below = self.next_id;
        updates(
            std::mem::take(&mut self.spawned),
            std::mem::take(&mut self.removed),
        )
    }
}

/// Packs spawns and removals into as few [`WorldItemUpdate`]s as the per-record
/// cap allows; no update when there is nothing to say.
pub fn updates(spawned: Vec<WorldItem>, removed: Vec<u64>) -> Vec<WorldItemUpdate> {
    use spall_protocol::limits::MAX_WORLD_ITEMS_PER_UPDATE as CAP;
    let mut out = Vec::new();
    let mut removed = removed.chunks(CAP);
    for chunk in spawned.chunks(CAP) {
        out.push(WorldItemUpdate {
            schema: WIRE_SCHEMA_VERSION,
            spawned: chunk.to_vec(),
            removed: Vec::new(),
        });
    }
    if let Some(last) = out.last_mut() {
        let room = CAP - last.spawned.len();
        if let Some(first) = removed.next() {
            let (fits, rest) = first.split_at(first.len().min(room));
            last.removed = fits.to_vec();
            if !rest.is_empty() {
                out.push(WorldItemUpdate {
                    schema: WIRE_SCHEMA_VERSION,
                    spawned: Vec::new(),
                    removed: rest.to_vec(),
                });
            }
        }
    }
    for chunk in removed {
        out.push(WorldItemUpdate {
            schema: WIRE_SCHEMA_VERSION,
            spawned: Vec::new(),
            removed: chunk.to_vec(),
        });
    }
    out
}

/// Warming at `position_m` from every lit fire among `items`, in degrees
/// Celsius of felt temperature: [`FIRE_HEAT_C`] at a fire's centre, staying
/// near that within arm's reach and falling to nothing at
/// [`FIRE_HEAT_RADIUS_M`] (`1 - (d / r)^2`), summed over fires. Unlit pits and other
/// items add nothing. Server warmth and the client's presented cold share it.
pub fn heat_at<'a>(items: impl IntoIterator<Item = &'a WorldItem>, position_m: [f64; 3]) -> f32 {
    let at = glam::DVec3::from_array(position_m);
    if !at.is_finite() {
        return 0.0;
    }
    items
        .into_iter()
        .filter(|item| item.is_lit())
        .map(|fire| {
            let centre =
                glam::DVec3::from_array(fire.translation_m) + glam::DVec3::Y * FIRE_CENTER_M;
            let d = centre.distance(at) as f32 / FIRE_HEAT_RADIUS_M;
            FIRE_HEAT_C * (1.0 - d * d).max(0.0)
        })
        .sum()
}

/// Where a fire set down from `eye` (metres) looking along `dir` rests: the
/// top of the solid cell the aim meets within [`FIRE_PLACE_REACH_M`], else
/// the ground [`FIRE_FRONT_M`] in front of the player. The spot must be on
/// the top face of solid ground, in reach and in sight of the eye, with open
/// air for the pit above it. `None` when there is no such spot.
pub fn fire_spot(volume: &Volume, eye: [f64; 3], dir: [f32; 3]) -> Option<[f64; 3]> {
    use glam::DVec3;
    use spall_voxel::query::{RayConfig, cast_ray};
    use spall_voxel::{Ray, RayOutcome};
    let cell_m = volume.cell_size().metres();
    let eye = DVec3::from_array(eye);
    let dir = DVec3::from_array(dir.map(f64::from));
    if !eye.is_finite() || !dir.is_finite() || dir.length_squared() < 1e-12 {
        return None;
    }
    // The top of the solid cell a ray from `from` meets within `reach_m`,
    // when the ray comes down onto that top face.
    let top_hit = |from: DVec3, along: DVec3, reach_m: f64| match cast_ray(
        volume,
        Ray::new(from / cell_m, along),
        RayConfig::new(reach_m / cell_m),
    ) {
        Ok(RayOutcome::Hit(hit))
            if hit.placement_cell.y == hit.cell.y + 1
                && hit.placement_cell.x == hit.cell.x
                && hit.placement_cell.z == hit.cell.z =>
        {
            Some(DVec3::new(
                (hit.cell.x as f64 + 0.5) * cell_m,
                (hit.cell.y + 1) as f64 * cell_m,
                (hit.cell.z as f64 + 0.5) * cell_m,
            ))
        }
        _ => None,
    };
    let spot = top_hit(eye, dir, FIRE_PLACE_REACH_M).or_else(|| {
        let flat = DVec3::new(dir.x, 0.0, dir.z).try_normalize()?;
        // The point in front must be in sight, not through a wall.
        let clear = matches!(
            cast_ray(
                volume,
                Ray::new(eye / cell_m, flat),
                RayConfig::new(FIRE_FRONT_M / cell_m),
            ),
            Ok(RayOutcome::Miss { .. })
        );
        clear.then_some(())?;
        top_hit(eye + flat * FIRE_FRONT_M, DVec3::NEG_Y, FIRE_PLACE_REACH_M)
    })?;
    if spot.distance(eye) > FIRE_PLACE_REACH_M {
        return None;
    }
    // Open air over the pit's footprint.
    let half = DVec3::new(FIRE_FOOTPRINT_HALF_M, 0.0, FIRE_FOOTPRINT_HALF_M);
    let lo = ((spot - half) / cell_m).floor().as_i64vec3();
    let hi = ((spot + half + DVec3::Y * FIRE_CLEARANCE_M) / cell_m)
        .ceil()
        .as_i64vec3();
    for x in lo.x..hi.x {
        for y in lo.y..hi.y {
            for z in lo.z..hi.z {
                if !matches!(
                    volume.sample(spall_core::GlobalCell::new(x, y, z)),
                    Ok(spall_voxel::Sample::Empty { .. })
                ) {
                    return None;
                }
            }
        }
    }
    Some(spot.to_array())
}

/// Whether solid cells lie within [`FIRE_SHELTER_M`] straight above `fire`.
pub(crate) fn sheltered(volume: &Volume, fire: &WorldItem) -> bool {
    let cell_m = volume.cell_size().metres();
    let [x, y, z] = fire.translation_m.map(|v| (v / cell_m).floor() as i64);
    let steps = (FIRE_SHELTER_M / cell_m).ceil() as i64;
    (1..=steps).any(|dy| {
        matches!(
            volume.sample(spall_core::GlobalCell::new(x, y + dy, z)),
            Ok(spall_voxel::Sample::Filled(_))
        )
    })
}

/// The nearest item within `reach_m` of `eye` (metres) that terrain does not
/// hide. Server pickup and the client prompt share this rule.
pub fn nearest_visible_item<'a>(
    volume: &Volume,
    eye: [f64; 3],
    reach_m: f64,
    items: impl IntoIterator<Item = &'a WorldItem>,
) -> Option<&'a WorldItem> {
    if !(eye.iter().all(|v| v.is_finite()) && reach_m.is_finite() && reach_m > 0.0) {
        return None;
    }
    let cell_m = volume.cell_size().metres();
    let origin = glam::DVec3::from_array(eye);
    let mut best: Option<(&WorldItem, f64)> = None;
    for item in items {
        // Aim a little above the item's resting point so the ray does not
        // graze the floor it lies on.
        let target = glam::DVec3::from_array(item.translation_m) + glam::DVec3::Y * 0.1;
        let d = target.distance(origin);
        if d >= reach_m || best.is_some_and(|(_, b)| d >= b) {
            continue;
        }
        let visible = d < 0.05
            || matches!(
                spall_voxel::query::cast_ray(
                    volume,
                    spall_voxel::Ray::new(origin / cell_m, target - origin),
                    spall_voxel::query::RayConfig::new((d - 0.05) / cell_m),
                ),
                Ok(spall_voxel::RayOutcome::Miss { .. })
            );
        if visible {
            best = Some((item, d));
        }
    }
    best.map(|(item, _)| item)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(item_id: u64, x: f64) -> WorldItem {
        WorldItem {
            item_id,
            kind: 1,
            translation_m: [x, 1.0, 0.0],
            yaw_rad: 0.0,
            fire_fuel_s: None,
        }
    }

    #[test]
    fn ids_are_never_reused_and_unannounced_spawns_vanish_quietly() {
        let mut items = WorldItems::default();
        let a = items.spawn(3, [0.0; 3], 0.0, None).unwrap();
        let b = items.spawn(3, [1.0; 3], 0.0, None).unwrap();
        assert_eq!((a.item_id, b.item_id), (1, 2));
        items.remove(b.item_id).unwrap();
        let changes = items.take_changes();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].spawned, vec![a]);
        assert!(changes[0].removed.is_empty());
        assert!(items.remove(b.item_id).is_none(), "taken once");
        items.remove(a.item_id).unwrap();
        assert_eq!(items.take_changes()[0].removed, vec![a.item_id]);
        assert_eq!(items.spawn(3, [0.0; 3], 0.0, None).unwrap().item_id, 3);
        assert_eq!(
            items.spawn(0, [0.0; 3], 0.0, None),
            Err(WorldItemError::Invalid),
            "kind 0 is not an item"
        );
    }

    #[test]
    fn restore_rejects_ids_at_or_past_the_counter_and_duplicates() {
        let mut items = WorldItems::default();
        assert_eq!(
            items.restore(2, vec![item(2, 0.0)]),
            Err(WorldItemError::BadId(2))
        );
        assert_eq!(
            items.restore(5, vec![item(1, 0.0), item(1, 1.0)]),
            Err(WorldItemError::BadId(1))
        );
        items.restore(5, vec![item(4, 0.0)]).unwrap();
        assert!(items.take_changes().is_empty());
        assert_eq!(items.spawn(1, [0.0; 3], 0.0, None).unwrap().item_id, 5);
    }

    #[test]
    fn large_change_sets_split_under_the_record_cap() {
        use spall_protocol::limits::MAX_WORLD_ITEMS_PER_UPDATE as CAP;
        let spawned: Vec<_> = (1..=CAP as u64 + 10).map(|i| item(i, 0.0)).collect();
        let removed: Vec<u64> = (5000..5000 + CAP as u64).collect();
        let out = updates(spawned, removed);
        assert!(out.iter().all(|u| u.spawned.len() + u.removed.len() <= CAP));
        assert_eq!(out.iter().map(|u| u.spawned.len()).sum::<usize>(), CAP + 10);
        assert_eq!(out.iter().map(|u| u.removed.len()).sum::<usize>(), CAP);
        assert!(updates(Vec::new(), Vec::new()).is_empty());
        assert_eq!(updates(Vec::new(), vec![1]).len(), 1);
    }

    /// Fire changes as one update's `spawned`, by id.
    fn announced(items: &mut WorldItems) -> Vec<WorldItem> {
        items
            .take_changes()
            .into_iter()
            .flat_map(|u| u.spawned)
            .collect()
    }

    #[test]
    fn a_fire_burns_a_second_of_fuel_per_second_and_goes_out_once() {
        let mut items = WorldItems::default();
        let fire = items.spawn(6, [0.0; 3], 0.0, Some(3.0)).unwrap();
        assert!(fire.is_lit());
        assert_eq!(announced(&mut items), vec![fire]);
        // 2.5 s at 60 Hz: still lit, and nothing to announce while burning.
        for _ in 0..150 {
            items.burn(1.0 / 60.0, |_| 1.0);
        }
        let left = items.iter().next().unwrap().fire_fuel_s.unwrap();
        assert!((left - 0.5).abs() < 1e-3, "{left}");
        assert!(announced(&mut items).is_empty());
        for _ in 0..60 {
            items.burn(1.0 / 60.0, |_| 1.0);
        }
        let out = announced(&mut items);
        assert_eq!(out.len(), 1, "going out is announced once");
        assert_eq!(out[0].fire_fuel_s, Some(0.0));
        assert!(!out[0].is_lit());
        assert!(
            items.iter().any(|i| i.item_id == fire.item_id),
            "the pit stays"
        );
        // A doubled rate burns twice as fast.
        items.add_fuel(fire.item_id, 1.0).unwrap();
        announced(&mut items);
        for _ in 0..30 {
            items.burn(1.0 / 60.0, |_| BLIZZARD_BURN_RATE);
        }
        assert!(!items.iter().next().unwrap().is_lit());
    }

    #[test]
    fn refuelling_relights_and_only_fires_take_fuel() {
        let mut items = WorldItems::default();
        let fire = items.spawn(6, [0.0; 3], 0.0, Some(0.0)).unwrap();
        let shovel = items.spawn(7, [1.0; 3], 0.0, None).unwrap();
        assert!(!fire.is_lit());
        announced(&mut items);
        assert_eq!(items.add_fuel(shovel.item_id, 10.0), None);
        assert_eq!(items.add_fuel(fire.item_id, -1.0), None);
        assert_eq!(items.add_fuel(fire.item_id, f32::NAN), None);
        let lit = items.add_fuel(fire.item_id, 180.0).unwrap();
        assert!(lit.is_lit());
        assert_eq!(lit.fire_fuel_s, Some(180.0));
        let again = items.add_fuel(fire.item_id, 600.0).unwrap();
        assert_eq!(again.fire_fuel_s, Some(780.0));
        // Two changes in one tick go out as one entry with the latest state.
        assert_eq!(announced(&mut items), vec![again]);
        // A changed fire taken away before announcing is still announced gone.
        items.add_fuel(fire.item_id, 1.0).unwrap();
        items.remove(fire.item_id).unwrap();
        let changes = items.take_changes();
        assert!(changes.iter().all(|u| u.spawned.is_empty()));
        assert_eq!(changes[0].removed, vec![fire.item_id]);
    }

    #[test]
    fn heat_falls_off_over_a_few_metres_and_sums_lit_fires() {
        let fire = |x: f64, fuel: f32| WorldItem {
            fire_fuel_s: Some(fuel),
            ..item(1, x)
        };
        let centre = |x: f64| [x, 1.0 + FIRE_CENTER_M, 0.0];
        let one = [fire(0.0, 60.0)];
        assert!((heat_at(&one, centre(0.0)) - FIRE_HEAT_C).abs() < 1e-4);
        let near = heat_at(&one, centre(1.0));
        let far = heat_at(&one, centre(3.0));
        assert!(
            FIRE_HEAT_C > near && near > far && far > 0.0,
            "{near} {far}"
        );
        assert_eq!(heat_at(&one, centre(f64::from(FIRE_HEAT_RADIUS_M))), 0.0);
        assert_eq!(heat_at(&one, centre(10.0)), 0.0);
        assert_eq!(heat_at(&[fire(0.0, 0.0)], centre(0.0)), 0.0, "unlit");
        assert_eq!(heat_at(&[item(1, 0.0)], centre(0.0)), 0.0, "not a fire");
        let two = [fire(-1.0, 60.0), fire(1.0, 60.0)];
        assert!((heat_at(&two, centre(0.0)) - 2.0 * near).abs() < 1e-4);
        assert_eq!(heat_at(&one, [f64::NAN, 0.0, 0.0]), 0.0);
    }

    /// The walk arena: floor top at y = 1 m, a 0.5 m ledge from x = 22 m.
    fn arena() -> Volume {
        crate::fixtures::walk_arena_setup().terrain
    }

    #[test]
    fn a_fire_rests_on_open_ground_in_reach() {
        let volume = arena();
        let eye = [2.0, 2.6, 2.0];
        // Aimed at the floor ahead: the top of the struck cell.
        let spot = fire_spot(&volume, eye, [1.0, -1.0, 0.0]).expect("floor in reach");
        assert_eq!(spot[1], 1.0);
        assert!((spot[0] - 3.625).abs() < 0.2, "{spot:?}");
        // Looking at the horizon: the ground in front of the player.
        let ahead = fire_spot(&volume, eye, [0.0, 0.0, 1.0]).expect("ground in front");
        assert_eq!(ahead[1], 1.0);
        assert!((ahead[2] - (2.0 + FIRE_FRONT_M)).abs() < 0.2, "{ahead:?}");
    }

    #[test]
    fn a_fire_is_refused_inside_terrain_or_out_of_reach() {
        let volume = arena();
        // Eye inside the floor.
        assert_eq!(fire_spot(&volume, [2.0, 0.5, 2.0], [1.0, -1.0, 0.0]), None);
        // Over a pit dug out of the floor up to x = 5 m: nothing below, and
        // the floor beyond is out of reach until the player steps closer.
        let mut dug = arena();
        let id = spall_core::VolumeId::new(1).unwrap();
        dug.apply_edit(&spall_voxel::EditPlan::filled_box(
            id,
            spall_core::GlobalCell::new(0, 0, 0),
            spall_core::GlobalCell::new(19, 3, 15),
            spall_core::MaterialId::AIR,
        ))
        .unwrap();
        assert_eq!(fire_spot(&dug, [2.0, 2.6, 2.0], [0.0, -1.0, 0.0]), None);
        assert_eq!(fire_spot(&dug, [2.0, 2.6, 2.0], [0.0, 0.0, 1.0]), None);
        assert_eq!(fire_spot(&dug, [2.0, 2.6, 2.0], [3.0, -1.6, 0.0]), None);
        assert!(fire_spot(&dug, [3.5, 2.6, 2.0], [1.6, -1.6, 0.0]).is_some());
        // Facing the ledge's side up close: no top face aimed at, and the
        // point in front is through the wall.
        assert_eq!(fire_spot(&volume, [21.7, 1.3, 2.0], [1.0, 0.0, 0.0]), None);
        // Floor right against the ledge: the pit would cut into it.
        assert_eq!(fire_spot(&volume, [20.5, 2.6, 2.0], [1.4, -1.6, 0.0]), None);
        // Bad aim.
        assert_eq!(fire_spot(&volume, [2.0, 2.6, 2.0], [0.0; 3]), None);
    }
}
