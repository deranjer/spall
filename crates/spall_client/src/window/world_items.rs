//! World items in the window (ENG-257): drawing each replicated item from the
//! game's model, and the E prompt for the nearest thing to use.

use super::*;
use spall_protocol::WorldItem;

/// Cubes for every item, posed at its translation and yaw.
pub(super) fn world_item_instances<'a>(
    items: impl Iterator<Item = &'a WorldItem>,
    view: &dyn PackView,
) -> Vec<Instance> {
    let mut out = Vec::new();
    for item in items {
        let rotation = glam::Quat::from_rotation_y(item.yaw_rad);
        let origin = Vec3::from_array(item.translation_m.map(|v| v as f32));
        out.extend(view.item_model(item.kind).iter().map(|voxel| {
            Instance::new(
                (origin + rotation * Vec3::from_array(voxel.center)).to_array(),
                voxel.material,
                voxel.size,
                rotation.to_array(),
            )
        }));
    }
    out
}

/// What E would use from `eye`: the nearest visible item in pickup reach,
/// else a nearby door. The point to label and the label. The server applies
/// the same item rule to its own player.
pub(super) fn interaction_prompt(
    replica: &crate::replica::ReplicaWorld,
    eye: [f64; 3],
    door: Option<spall_voxel::doors::DoorSpec>,
    view: Option<&dyn PackView>,
) -> Option<([f64; 3], String)> {
    let volume = replica.terrain_volume()?;
    let reach = spall_sim::world_items::ITEM_PICKUP_REACH_M;
    if let Some(item) =
        spall_sim::world_items::nearest_visible_item(volume, eye, reach, replica.world_items())
    {
        let name = view
            .and_then(|view| view.item_name(item.kind))
            .unwrap_or("item");
        let mut at = item.translation_m;
        at[1] += 0.2;
        return Some((at, format!("E  Pick up {name}")));
    }
    let door = spall_voxel::doors::nearby_door(volume, eye, door?, 3.0)?;
    let label = if door.open { "E  Close" } else { "E  Open" };
    Some((door.center_m(), label.to_string()))
}
