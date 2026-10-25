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
    time_s: f32,
    eye: Option<[f64; 3]>,
) -> Vec<Instance> {
    let mut out = Vec::new();
    for item in items {
        let rotation = glam::Quat::from_rotation_y(item.yaw_rad);
        let origin = Vec3::from_array(item.translation_m.map(|v| v as f32));
        let lit = match item.fire_fuel_s {
            Some(fuel_s) if item.is_lit() && fire_effect_in_range(item, eye) => {
                view.lit_model(item.kind, fuel_s, time_s, item.item_id)
            }
            _ => Vec::new(),
        };
        out.extend(view.item_model(item.kind).iter().chain(&lit).map(|voxel| {
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

/// Beyond this distance from the eye a lit fire's animated flame, embers and
/// smoke are not built (the pit and the light still are).
const FIRE_EFFECT_RANGE_M: f64 = 64.0;

/// Whether `fire` is near enough to `eye` to animate. Without an eye (no
/// view yet) every fire animates.
fn fire_effect_in_range(fire: &WorldItem, eye: Option<[f64; 3]>) -> bool {
    eye.is_none_or(|eye| {
        let d: f64 = (0..3)
            .map(|i| (fire.translation_m[i] - eye[i]).powi(2))
            .sum();
        d <= FIRE_EFFECT_RANGE_M * FIRE_EFFECT_RANGE_M
    })
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

/// A lit fire's light intensity multiplier at `time_s`, about `0.85..=1.0`: a
/// slow wobble of two sines, a different phase for every fire.
fn light_flicker(time_s: f32, item_id: u64) -> f32 {
    let phase = (item_id % 1024) as f32 * 0.61;
    let wobble = (time_s * 11.0 + phase).sin() + (time_s * 25.3 + phase * 1.7).sin();
    0.925 + 0.075 * 0.5 * wobble
}

/// A point light at the centre of every lit fire among `fires`, flickering
/// with `time_s`.
pub(super) fn fire_lights(fires: &[WorldItem], time_s: f32) -> Vec<spall_render::PointLight> {
    fires
        .iter()
        .filter(|fire| fire.is_lit())
        .map(|fire| {
            let mut at = fire.translation_m;
            at[1] += spall_sim::world_items::FIRE_CENTER_M;
            let flicker = light_flicker(time_s, fire.item_id);
            spall_render::PointLight {
                position: at.map(|v| v as f32),
                color: FIRE_LIGHT.map(|c| c * flicker),
                radius: FIRE_LIGHT_RADIUS_M,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fire(id: u64, fuel: f32) -> WorldItem {
        WorldItem {
            item_id: id,
            kind: 6,
            translation_m: [0.0; 3],
            yaw_rad: 0.0,
            fire_fuel_s: Some(fuel),
        }
    }

    /// A view whose lit fire is `n` cubes, to count what the window asks for.
    #[derive(Debug)]
    struct Flame(usize);
    impl PackView for Flame {
        fn catalog_version(&self) -> u32 {
            0
        }
        fn item_name(&self, _: u16) -> Option<&str> {
            None
        }
        fn item_icon_png(&self, _: u16) -> Option<&'static [u8]> {
            None
        }
        fn blueprints(&self) -> &[Blueprint] {
            &[]
        }
        fn lit_model(&self, _: u16, _: f32, _: f32, _: u64) -> Vec<ItemVoxel> {
            vec![
                ItemVoxel {
                    center: [0.0; 3],
                    size: [0.1; 3],
                    material: 1,
                    tint: None,
                };
                self.0
            ]
        }
    }

    #[test]
    fn flames_draw_only_for_lit_fires_in_range() {
        let view = Flame(5);
        let count = |fire: &WorldItem, eye| {
            world_item_instances(std::iter::once(fire), &view, 0, 1.0, eye).len()
        };
        let (lit, out) = (fire(1, 60.0), fire(2, 0.0));
        assert_eq!(count(&lit, Some([5.0, 0.0, 0.0])), 5);
        assert_eq!(count(&out, Some([5.0, 0.0, 0.0])), 0);
        assert_eq!(count(&lit, Some([100.0, 0.0, 0.0])), 0);
        assert_eq!(count(&lit, None), 5);
    }

    #[test]
    fn a_fire_light_flickers_but_only_while_lit() {
        let lit = fire(3, 60.0);
        let reds: Vec<f32> = (0..200)
            .map(|i| fire_lights(&[lit], i as f32 * 0.05)[0].color[0])
            .collect();
        let lo = reds.iter().copied().fold(f32::MAX, f32::min);
        let hi = reds.iter().copied().fold(f32::MIN, f32::max);
        assert!(hi - lo > 0.05 * FIRE_LIGHT[0]);
        assert!(lo >= FIRE_LIGHT[0] * 0.84 && hi <= FIRE_LIGHT[0] * 1.001);
        assert!(fire_lights(&[fire(4, 0.0)], 1.0).is_empty());
    }
}
