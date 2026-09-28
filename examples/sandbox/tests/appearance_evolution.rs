//! Palette evolution is safe: variant identities are explicit and append-only,
//! historical manifests are frozen, extending the palette keeps every earlier
//! id/name/base/colour, and an earlier saved world restores unchanged under the
//! extended manifest while a renumbered or recoloured one is refused. See
//! `docs/reports/ENG-95.md`.

use std::collections::BTreeSet;
use std::path::Path;

use sandbox::appearance::{self, EXTENSION_ID_BASE, Variant};
use sandbox::editor_scene;
use sandbox::game::{self, materials};
use spall_core::{GlobalCell, MaterialId, MaterialManifest};
use spall_protocol::content_manifest_hash;
use spall_sim::{Simulation, SimulationConfig};
use spall_voxel::Sample;

fn hash_bytes(manifest: &MaterialManifest) -> [u8; 32] {
    content_manifest_hash(manifest).0
}

/// Pinned content hashes of every shipped manifest version. These are the
/// definitions saved worlds and replicas were stamped with; a change to any of
/// them means history was rewritten.
const V1: [u8; 32] = [
    228, 84, 41, 144, 115, 135, 120, 18, 198, 97, 168, 184, 178, 118, 108, 50, 39, 120, 136, 118,
    199, 111, 19, 182, 53, 195, 212, 210, 165, 162, 131, 194,
];
const V2: [u8; 32] = [
    153, 93, 170, 22, 230, 144, 190, 92, 7, 90, 239, 252, 134, 237, 29, 47, 93, 64, 14, 85, 105,
    140, 153, 125, 77, 246, 96, 143, 50, 239, 177, 241,
];
const V3: [u8; 32] = [
    241, 174, 109, 124, 130, 133, 27, 160, 204, 140, 108, 129, 200, 100, 47, 227, 94, 116, 128, 59,
    201, 59, 164, 30, 1, 65, 126, 251, 164, 222, 139, 188,
];
const V4: [u8; 32] = [
    251, 66, 81, 172, 209, 193, 147, 228, 181, 190, 94, 193, 18, 244, 26, 205, 14, 39, 225, 36, 78,
    40, 16, 99, 13, 147, 82, 3, 243, 80, 101, 216,
];

fn forest() -> &'static Path {
    Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/terrain-trees-forest"
    ))
}

/// A plausible palette growth: two new colours for existing keys and a first
/// tint for a key that had none, under ids the generator would allocate.
fn grown_palette() -> Vec<Variant> {
    let mapping = editor_scene::material_mapping();
    let row = |id: u16, key: &str, name: &str, srgb: [u8; 3]| Variant {
        id: MaterialId(id),
        base: mapping[key],
        name: name.to_owned(),
        srgb,
    };
    vec![
        row(EXTENSION_ID_BASE, "grass", "grass.v12", [30, 160, 30]),
        row(
            EXTENSION_ID_BASE + 1,
            "wood.oak",
            "wood.oak.v12",
            [70, 50, 30],
        ),
        row(
            EXTENSION_ID_BASE + 2,
            "stone.granite",
            "stone.granite.v00",
            [130, 130, 135],
        ),
    ]
}

#[test]
fn shipped_manifests_are_frozen() {
    assert_eq!(hash_bytes(&game::legacy_manifest_v1()), V1);
    assert_eq!(hash_bytes(&game::manifest_v2_linear()), V2);
    assert_eq!(
        hash_bytes(&game::manifest_v3_variants()),
        V3,
        "the v3 variants come from the frozen table, never the mutable palette"
    );
    assert_eq!(hash_bytes(&game::manifest_v4_lamp()), V4);

    // The frozen rows are what the v3 manifest holds, by explicit id.
    let v3 = game::manifest_v3_variants();
    let frozen = appearance::frozen_v3_variants();
    assert_eq!(frozen.len(), 37);
    for (offset, variant) in frozen.iter().enumerate() {
        assert_eq!(variant.id.0, 100 + offset as u16);
        assert_eq!(v3.get(variant.id).unwrap().name, variant.name);
    }
    // No extensions are committed yet, so the current manifest is v4.
    assert!(appearance::extension_variants().is_empty());
    assert_eq!(hash_bytes(&game::manifest()), V4);
}

#[test]
fn variant_ids_are_unique_and_never_collide_with_reserved_materials() {
    let current = game::manifest();
    let mut ids = BTreeSet::new();
    for variant in appearance::variants() {
        assert!(ids.insert(variant.id.0), "duplicate id {}", variant.id.0);
        assert!(
            !current
                .entries()
                .iter()
                .any(|def| def.id == variant.id && def.name != variant.name),
            "{} collides with another material",
            variant.name
        );
    }
    for variant in appearance::extension_variants() {
        assert!(variant.id.0 >= EXTENSION_ID_BASE);
    }
    assert!(
        EXTENSION_ID_BASE > materials::LAMP.0,
        "extensions live above the lamp"
    );
    // The generator's constant agrees (it cannot depend on this crate).
    let generator = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tools/spall_editor/examples/appearance_palette.rs"),
    )
    .unwrap();
    assert!(
        generator.contains(&format!("FIRST_EXTENSION_ID: u16 = {EXTENSION_ID_BASE};")),
        "generator and sandbox must agree on the extension id base"
    );
}

#[test]
fn extending_the_palette_keeps_every_earlier_meaning_and_colour() {
    let v4 = game::manifest_v4_lamp();
    let grown = game::extend_with_variants(&v4, grown_palette()).expect("an additive extension");
    assert_eq!(grown.len(), v4.len() + 3);
    for before in v4.entries() {
        let now = grown.get(before.id).expect("nothing removed");
        assert_eq!(now.name, before.name, "id {} renamed", before.id.0);
        assert_eq!(now.sim, before.sim, "{} changed behaviour", before.name);
        assert_eq!(now.render, before.render, "{} recoloured", before.name);
    }
    // New entries sit above the lamp and behave as their base.
    for variant in grown_palette() {
        let def = grown.get(variant.id).unwrap();
        assert!(def.id.0 > materials::LAMP.0);
        assert_eq!(def.sim, grown.get(variant.base).unwrap().sim);
    }
    // Every earlier manifest is a restorable predecessor of the grown one.
    let predecessors: Vec<[u8; 32]> = grown
        .appearance_predecessors()
        .iter()
        .map(hash_bytes)
        .collect();
    for pinned in [V1, V2, V3, V4] {
        assert!(predecessors.contains(&pinned));
    }
}

#[test]
fn a_reused_lower_or_recoloured_row_is_refused() {
    let v4 = game::manifest_v4_lamp();
    let mut collide = grown_palette();
    collide[0].id = materials::LAMP;
    assert!(
        game::extend_with_variants(&v4, collide).is_err(),
        "an id that collides with the lamp"
    );
    let mut reuse = grown_palette();
    reuse[0].id = MaterialId(105);
    assert!(
        game::extend_with_variants(&v4, reuse).is_err(),
        "an existing variant's id"
    );
    let mut low = grown_palette();
    low[0].id = MaterialId(150);
    assert!(
        game::extend_with_variants(&v4, low).is_err(),
        "a free id below the current highest is not an extension"
    );

    // The old failure mode: a regenerated, re-sorted table reassigns colours to
    // ids. Swap two variants' colours in a manifest and it is not accepted as
    // an extension of v4.
    let mut entries = v4.entries().to_vec();
    let (a, b) = (
        entries.iter().position(|d| d.id.0 == 100).unwrap(),
        entries.iter().position(|d| d.id.0 == 101).unwrap(),
    );
    let colour = entries[a].render.albedo;
    entries[a].render.albedo = entries[b].render.albedo;
    entries[b].render.albedo = colour;
    assert!(
        MaterialManifest::validated(entries)
            .unwrap()
            .superseding_extension(&v4)
            .is_err(),
        "recolouring existing variants is not an extension"
    );
}

fn material_at(sim: &Simulation, cell: GlobalCell) -> Sample {
    sim.world()
        .volume_ref(sim.world().terrain_volume_id())
        .unwrap()
        .sample(cell)
        .unwrap()
}

/// A world saved under today's manifest restores under a grown palette with
/// identical material ids, and each id keeps its colour; it is refused under a
/// manifest that recoloured a variant.
#[test]
fn a_saved_world_restores_under_an_extended_palette_unchanged() {
    let scene = editor_scene::load(forest()).expect("forest loads");
    let sim = Simulation::new(SimulationConfig::new(scene.world_setup())).expect("world");
    let mut sample = Vec::new();
    for x in (0..160).step_by(3) {
        for z in (0..160).step_by(3) {
            for y in 4..14 {
                let cell = GlobalCell::new(x, y, z);
                if let Sample::Filled(m) = material_at(&sim, cell) {
                    sample.push((cell, m));
                }
            }
        }
    }
    assert!(
        sample
            .iter()
            .any(|(_, m)| m.0 >= appearance::VARIANT_ID_BASE && m.0 != materials::LAMP.0),
        "the forest holds tinted cells"
    );

    let dir = std::env::temp_dir().join(format!("spall_palette_evolution_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("world.db");
    let cfg = spall_server::persist::PersistConfig {
        world_id: 0xA99E_0000_0000_0195,
        seed: 1,
        generator_version: 1,
    };
    let checkpoint = spall_server::persist::capture(&sim, &cfg, 0).expect("capture");
    spall_store::Writer::open(&db)
        .unwrap()
        .publish_checkpoint(&checkpoint)
        .unwrap();
    let recovery = spall_store::recover(&db).unwrap();
    let restore = |manifest: MaterialManifest| {
        spall_server::persist::restore(
            &recovery,
            &cfg,
            spall_server::persist::RecoveryChoice::RequireClean,
            manifest,
            spall_structure::AnchorPlane::at(0),
            spall_physics::PhysicsConfig::default(),
        )
    };

    let grown = game::extend_with_variants(&game::manifest(), grown_palette()).expect("extension");
    let (restored, _) = restore(grown.clone()).expect("restores under the extended palette");
    for (cell, before) in &sample {
        assert_eq!(
            material_at(&restored, *cell),
            Sample::Filled(*before),
            "{cell:?} changed meaning under the extended palette"
        );
        // And the id still resolves to the colour it always had.
        assert_eq!(
            grown.get(*before).unwrap().render,
            game::manifest().get(*before).unwrap().render
        );
    }

    // A recoloured variant is a different world: refused, not silently accepted.
    let mut recoloured = game::manifest().entries().to_vec();
    let target = recoloured.iter().position(|d| d.id.0 == 100).unwrap();
    recoloured[target].render.albedo = [0.9, 0.1, 0.1];
    let recoloured = MaterialManifest::validated(recoloured).unwrap();
    assert!(
        restore(recoloured).is_err(),
        "a manifest that recoloured a variant must not restore the old world"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
