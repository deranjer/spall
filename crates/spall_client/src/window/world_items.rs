//! World items in the window (ENG-257): drawing each replicated item from the
//! game's model, and the E prompt for the nearest thing to use. Fires
//! (ENG-266) also draw the game's lit model and cast a point light while lit.

use super::*;
use spall_protocol::WorldItem;

/// Cubes for every item, posed at its translation and yaw. A tinted voxel
/// draws render material `tint_base + tint` (see [`PackView::item_tints`]).
pub(super) fn world_item_instances<'a>(
    items: impl Iterator<Item = &'a WorldItem>,
    view: &dyn PackView,
    tint_base: u32,
) -> Vec<Instance> {
    let mut out = Vec::new();
    for item in items {
        let rotation = glam::Quat::from_rotation_y(item.yaw_rad);
        let origin = Vec3::from_array(item.translation_m.map(|v| v as f32));
        let lit: &[ItemVoxel] = if item.is_lit() {
            view.lit_model(item.kind)
        } else {
            &[]
        };
        out.extend(view.item_model(item.kind).iter().chain(lit).map(|voxel| {
            Instance::new(
                (origin + rotation * Vec3::from_array(voxel.center)).to_array(),
                voxel
                    .tint
                    .map_or(voxel.material, |tint| tint_base + u32::from(tint)),
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
        let mut at = item.translation_m;
        at[1] += 0.2;
        let label = match item.fire_fuel_s {
            // The server feeds a fire from the pack instead of picking it up.
            Some(_) if item.is_lit() => "E  Add fuel".to_string(),
            Some(_) => "E  Add fuel and relight".to_string(),
            None => {
                let name = view
                    .and_then(|view| view.item_name(item.kind))
                    .unwrap_or("item");
                format!("E  Pick up {name}")
            }
        };
        return Some((at, label));
    }
    let door = spall_voxel::doors::nearby_door(volume, eye, door?, 3.0)?;
    let label = if door.open { "E  Close" } else { "E  Open" };
    Some((door.center_m(), label.to_string()))
}

/// Colour times intensity, and reach in metres, of a lit fire's point light:
/// warmer and a little stronger than a torch.
const FIRE_LIGHT: [f32; 3] = [32.0, 15.0, 4.5];
const FIRE_LIGHT_RADIUS_M: f32 = 16.0;

/// A point light at the centre of every lit fire among `fires`.
pub(super) fn fire_lights(fires: &[WorldItem]) -> Vec<spall_render::PointLight> {
    fires
        .iter()
        .filter(|fire| fire.is_lit())
        .map(|fire| {
            let mut at = fire.translation_m;
            at[1] += spall_sim::world_items::FIRE_CENTER_M;
            spall_render::PointLight {
                position: at.map(|v| v as f32),
                color: FIRE_LIGHT,
                radius: FIRE_LIGHT_RADIUS_M,
            }
        })
        .collect()
}
