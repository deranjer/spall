//! The Winter Crossing sound table (ENG-271): user-approved recordings under
//! `scenario/`, with their loudness and reach. Recordings are peak-normalised
//! to about -3 dB, so these gains compare directly.

use std::path::Path;

use spall_client::scenario_sounds::{Clip, DigRule, PickupRule, ScenarioSounds};
use spall_core::MaterialId;

use crate::appearance::variants;
use crate::game::{materials, tool_ids};

fn clip(root: &Path, names: &[&str], gain: f32, range_m: f32) -> Clip {
    Clip {
        files: names
            .iter()
            .map(|name| root.join("scenario").join(format!("{name}.ogg")))
            .collect(),
        gain,
        rate: 1.0,
        range_m,
    }
}

/// `base` and every appearance variant of it.
fn with_variants(base: MaterialId) -> Vec<MaterialId> {
    std::iter::once(base)
        .chain(
            variants()
                .into_iter()
                .filter(|v| v.base == base)
                .map(|v| v.id),
        )
        .collect()
}

pub fn sounds(root: &Path) -> ScenarioSounds {
    let one = |name: &str, gain: f32| clip(root, &[name], gain, 0.0);
    let rule = |tool, materials: Vec<MaterialId>, clip| DigRule {
        tool,
        materials,
        clip,
    };
    let pickup = |words: &[&str], clip| PickupRule {
        words: words.iter().map(|word| (*word).to_owned()).collect(),
        clip,
    };
    let stones: Vec<MaterialId> = [
        materials::STONE,
        materials::DEEP_STONE,
        materials::BEDROCK,
        materials::GRAVEL,
    ]
    .into_iter()
    .flat_map(with_variants)
    .collect();
    ScenarioSounds {
        // Fires carry as far as their light does; the bed is the crackle.
        fire_loop: clip(root, &["fire-crackle-loop"], 0.55, 18.0),
        fire_ignite: clip(
            root,
            &["fire-ignite-match", "fire-ignite-strike"],
            0.8,
            30.0,
        ),
        fire_flare: clip(root, &["fire-flare"], 0.7, 30.0),
        fire_out: clip(root, &["fire-out-hiss", "fire-out-steam"], 0.7, 30.0),
        ice_materials: [materials::ICE, materials::ICE_CRACKED].to_vec(),
        ice_cracked: Some(materials::ICE_CRACKED),
        ice_creak: clip(
            root,
            &["ice-creak-short", "ice-creak-lake", "ice-creak-foot"],
            0.35,
            0.0,
        ),
        ice_crack: clip(root, &["ice-crack-sharp", "ice-crack-short"], 0.9, 60.0),
        ice_break: clip(root, &["ice-break-through"], 0.9, 60.0),
        // The user found the splash a little high pitched: play it lower.
        splash: Clip {
            rate: 0.88,
            ..clip(root, &["ice-splash", "ice-splash-heavy"], 0.8, 0.0)
        },
        gasp: one("cold-gasp", 0.7),
        underwater_loop: one("underwater-loop", 0.6),
        digs: vec![
            rule(
                tool_ids::SHOVEL,
                with_variants(materials::SNOW),
                clip(root, &["shovel-snow-a", "shovel-snow-b"], 0.7, 0.0),
            ),
            rule(
                tool_ids::SHOVEL,
                stones,
                clip(root, &["shovel-stones"], 0.7, 0.0),
            ),
            rule(tool_ids::SHOVEL, Vec::new(), one("shovel-dirt", 0.7)),
            rule(
                tool_ids::AXE,
                with_variants(materials::WOOD),
                clip(
                    root,
                    &[
                        "axe-chop-a",
                        "axe-chop-b",
                        "axe-chop-c",
                        "log-split-a",
                        "log-split-b",
                    ],
                    0.8,
                    0.0,
                ),
            ),
        ],
        tree_fall: clip(root, &["tree-fall"], 0.9, 80.0),
        pickups: vec![
            pickup(&["stone", "rock"], one("pickup-stone", 0.7)),
            pickup(&["stick", "log", "wood", "plank"], one("pickup-wood", 0.7)),
            pickup(&[], one("pickup-item", 0.5)),
        ],
        note: one("note-unfold", 0.6),
        door_open: one("door-open", 0.7),
        door_latch: one("door-latch", 0.6),
        door_close: one("door-close", 0.8),
        // Lower than the rest: the user found the climb a bit loud. A slide
        // is the climb recording played faster.
        ladder_climb: one("ladder-climb", 0.4),
        ladder_slide: Clip {
            rate: 1.7,
            ..one("ladder-climb", 0.4)
        },
        shiver_loop: one("teeth-chatter-loop", 0.5),
        hurt: one("hurt", 0.8),
        death: one("death-fall", 0.9),
        heartbeat_slow: one("heartbeat-slow-loop", 0.6),
        heartbeat_fast: one("heartbeat-fast-loop", 0.7),
        ui_click: one("ui-click", 0.5),
    }
}
