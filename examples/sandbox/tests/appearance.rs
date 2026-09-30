//! Authored tint survives as material identity: the manifest extends cleanly,
//! the forest's authored greens map to appearance variants within a measured
//! error, variants keep their base's rules, and the variant ids survive an
//! edit, a body split and a save/restore. See `docs/reports/ENG-95.md`.

use std::collections::BTreeMap;
use std::path::Path;

use sandbox::appearance::{self, Resolver, VARIANT_ID_BASE};
use sandbox::editor_scene;
use sandbox::game::{self, materials};
use spall_core::units::{BRUSH_UNIT, BrushPoint};
use spall_core::{EntityId, GlobalCell, MaterialId, SphereBrush};
use spall_protocol::RequestId;
use spall_protocol::content_manifest_hash;
use spall_sim::{EditIntent, EditTarget, Simulation, SimulationConfig};
use spall_voxel::Sample;

fn forest() -> &'static Path {
    Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/terrain-trees-forest"
    ))
}

fn srgb_to_linear(channel: u8) -> f32 {
    let c = f32::from(channel) / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

#[test]
fn the_manifest_extends_the_linear_one_and_variants_keep_their_bases_rules() {
    let (current, v2, v1) = (
        game::manifest(),
        game::manifest_v2_linear(),
        game::legacy_manifest_v1(),
    );
    let hashes: Vec<_> = current
        .appearance_predecessors()
        .iter()
        .map(content_manifest_hash)
        .collect();
    assert!(
        hashes.contains(&content_manifest_hash(&v2)),
        "worlds saved under v2 must restore"
    );
    assert!(
        hashes.contains(&content_manifest_hash(&v1)),
        "worlds saved under v1 must restore"
    );
    assert_ne!(content_manifest_hash(&current), content_manifest_hash(&v2));

    let variants = appearance::variants();
    assert!(
        variants.len() >= 30,
        "expected a real palette, got {}",
        variants.len()
    );
    // v2 + the appearance variants + the emissive lamp (ENG-102).
    assert_eq!(current.len(), v2.len() + variants.len() + 1);
    let mut names = std::collections::BTreeSet::new();
    for variant in &variants {
        assert!(variant.id.0 >= VARIANT_ID_BASE);
        assert!(
            names.insert(variant.name.clone()),
            "duplicate {}",
            variant.name
        );
        let (def, base) = (
            current.get(variant.id).unwrap(),
            current.get(variant.base).unwrap(),
        );
        assert_eq!(
            def.sim, base.sim,
            "{} must behave exactly like its base",
            variant.name
        );
        assert_eq!(
            (def.render.roughness, def.render.metalness),
            (base.render.roughness, base.render.metalness)
        );
        assert_eq!(
            def.render.albedo,
            variant.srgb.map(srgb_to_linear),
            "albedo is the authored colour, linear"
        );
        assert_eq!(appearance::base_material(variant.id), variant.base);
    }
    // The lamp glows: emission is albedo x 5 in the renderer's encoding, and it is solid.
    let lamp = current.get(materials::LAMP).unwrap();
    assert!(lamp.sim.flags.contains(spall_core::MaterialFlags::COLLIDES));
    let emitted = lamp
        .render
        .albedo
        .iter()
        .zip(lamp.render.emissive)
        .all(|(a, e)| (a * 5.0 - e).abs() < 1e-3);
    assert!(emitted, "{lamp:?}");
    // Every original material is unchanged in identity and rules.
    for before in v2.entries() {
        let now = current.get(before.id).unwrap();
        assert_eq!((&now.name, now.sim), (&before.name, before.sim));
    }
}

#[test]
fn wood_variants_still_drop_wood() {
    let wood_variant = appearance::variants()
        .into_iter()
        .find(|v| v.base == materials::WOOD)
        .expect("the palette has wood variants")
        .id;
    let removed: BTreeMap<MaterialId, u64> = [(wood_variant, 20), (materials::WOOD, 12)].into();
    assert_eq!(appearance::merge_variants(&removed)[&materials::WOOD], 32);
    let mut inventories = game::PlayerInventories::default();
    let drops = inventories.record_committed_cut(0, &removed).unwrap();
    // 32 wood cells (variant + base together) -> 2 logs.
    assert_eq!(drops.iter().map(|d| d.count).sum::<u32>(), 2, "{drops:?}");
}

/// The forest's authored colours, checked cell by cell: each tinted cell maps to
/// a variant (never silently to the base), within a small measured distance of
/// what the artist painted.
#[test]
fn forest_tints_map_to_variants_within_the_measured_error() {
    let mapping = editor_scene::material_mapping();
    let resolver = Resolver::default();
    let mut per_key: BTreeMap<String, (f64, f64, u64)> = BTreeMap::new();
    let project = spall_editor_free_assets();
    let mut tinted = 0u64;
    for bytes in &project {
        let asset =
            sandbox::content::decode_editor_voxels(sandbox::content::AssetId(1), bytes, &mapping)
                .expect("fixture asset decodes");
        for cell in &asset.cells {
            let Some(tint) = cell.tint else { continue };
            tinted += 1;
            let id = resolver.resolve(cell.material, Some(tint));
            assert!(
                id.0 >= VARIANT_ID_BASE,
                "a tinted {:?} cell resolved to its base",
                cell.material
            );
            let chosen = resolver.srgb_of(id).unwrap();
            let error = (0..3)
                .map(|c| (f64::from(tint[c]) - f64::from(chosen[c])).powi(2))
                .sum::<f64>()
                .sqrt();
            let entry = per_key.entry(format!("{:?}", cell.material)).or_default();
            entry.0 += error;
            entry.1 = entry.1.max(error);
            entry.2 += 1;
        }
    }
    assert!(
        tinted > 10_000,
        "the fixture assets carry authored tints: {tinted}"
    );
    for (material, (sum, worst, cells)) in &per_key {
        let mean = sum / *cells as f64;
        println!(
            "{material}: {cells} tinted cells, mean error {mean:.2}/255 (Euclid sRGB), worst {worst:.1}"
        );
        assert!(
            mean < 6.0,
            "{material}: authored colours drift too far ({mean})"
        );
        assert!(*worst < 50.0, "{material}: worst-case colour error {worst}");
    }
}

/// Raw bytes of the checked-in tinted assets.
fn spall_editor_free_assets() -> Vec<Vec<u8>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
    let mut out = Vec::new();
    for project in ["terrain-trees", "terrain-trees-v2", "terrain-trees-forest"] {
        let dir = root.join(project).join("assets");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut files: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        files.sort();
        out.extend(
            files
                .into_iter()
                .filter(|p| p.extension().is_some_and(|e| e == "spvox"))
                .filter_map(|p| std::fs::read(p).ok()),
        );
    }
    out
}

fn brush_at(cell: GlobalCell, radius_cells: i64) -> SphereBrush {
    let half = BRUSH_UNIT / 2;
    SphereBrush::new(
        BrushPoint::from_units(
            cell.x * BRUSH_UNIT + half,
            cell.y * BRUSH_UNIT + half,
            cell.z * BRUSH_UNIT + half,
        ),
        radius_cells * BRUSH_UNIT,
    )
    .expect("valid brush")
}

fn material_at(sim: &Simulation, cell: GlobalCell) -> Sample {
    sim.world()
        .volume_ref(sim.world().terrain_volume_id())
        .unwrap()
        .sample(cell)
        .unwrap()
}

/// End to end on the real forest: cut a trunk, and the variants of the cells
/// that remain, and of the cells that become a detached body, are unchanged;
/// then save and restore and they are still unchanged.
#[test]
fn variants_survive_an_edit_a_body_split_and_a_save() {
    let scene = editor_scene::load(forest()).expect("forest loads");
    let mut sim = Simulation::new(SimulationConfig::new(scene.world_setup())).expect("world");

    // A wood-variant cell well above the lawn: part of a trunk.
    let trunk = (0..160)
        .flat_map(|x| (0..160).map(move |z| (x, z)))
        .flat_map(|(x, z)| (6..12).map(move |y| GlobalCell::new(x, y, z)))
        .find(|cell| {
            matches!(material_at(&sim, *cell), Sample::Filled(m) if m.0 >= VARIANT_ID_BASE && appearance::base_material(m) == materials::WOOD)
        })
        .expect("the forest has tinted wood above the lawn");

    // Snapshot every solid cell within 6 m of the trunk that is outside the cut.
    let cut = brush_at(trunk, 2);
    let mut kept = Vec::new();
    for dx in -24..=24 {
        for dy in -8..=24 {
            for dz in -24..=24 {
                let cell = GlobalCell::new(trunk.x + dx, trunk.y + dy, trunk.z + dz);
                if dx.abs() <= 3 && dz.abs() <= 3 && dy.abs() <= 3 {
                    continue; // inside or beside the cut
                }
                if let Sample::Filled(m) = material_at(&sim, cell) {
                    kept.push((cell, m));
                }
            }
        }
    }
    assert!(
        kept.iter().any(|(_, m)| m.0 >= VARIANT_ID_BASE),
        "the neighbourhood has variants"
    );

    sim.submit(EditIntent::cut(
        RequestId(1),
        EntityId::new(1).unwrap(),
        EditTarget::Terrain,
        cut,
    ))
    .expect("submit");
    sim.run_until_idle(64).expect("settle");
    let committed = sim.committed(RequestId(1)).expect("the cut committed");
    // Regression: hundreds of small detached children used to be re-encoded
    // inline (each blob under its cap) into a 100 KB record, over the 64 KiB
    // control limit, which made the server drop every live client.
    let wire = spall_protocol::encode_control(&committed.topology)
        .expect("the trunk cut's topology transaction fits one reliable control record");
    println!("trunk cut topology record: {} bytes", wire.len());

    // The cut split something off, and every cell of every body is a real,
    // known material -- variants preserved, never reset to a base or air.
    let bodies: Vec<_> = sim
        .world()
        .bodies()
        .filter(|b| b.entity.is_some())
        .collect();
    let mut body_variants = 0;
    for body in &bodies {
        let volume = &body.volume;
        for coord in volume.resident_brick_coords() {
            for index in 0..32_768u32 {
                if let Ok(Sample::Filled(m)) = volume.sample_local(coord, index) {
                    assert!(
                        game::manifest().contains(m),
                        "unknown material {m:?} in a body"
                    );
                    body_variants += usize::from(m.0 >= VARIANT_ID_BASE);
                }
            }
        }
    }
    println!(
        "{} detached bodies carry {body_variants} variant cells",
        bodies.len()
    );
    assert!(
        !bodies.is_empty() && body_variants > 0,
        "the split carried tinted cells with it"
    );

    // Every cell that is still terrain holds exactly the variant it started
    // with. A cell that left the terrain is either inside the cut or moved into
    // a detached body, so per material the cells that moved out cannot outnumber
    // the cells the bodies hold.
    let centre = [trunk.x as f64, trunk.y as f64, trunk.z as f64];
    let in_cut = |cell: &GlobalCell| {
        let d = [
            cell.x as f64 - centre[0],
            cell.y as f64 - centre[1],
            cell.z as f64 - centre[2],
        ];
        (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt() <= 2.6
    };
    let mut moved: BTreeMap<MaterialId, u64> = BTreeMap::new();
    for (cell, before) in &kept {
        match material_at(&sim, *cell) {
            Sample::Filled(now) => assert_eq!(now, *before, "{cell:?} changed"),
            _ if in_cut(cell) => {}
            _ => *moved.entry(*before).or_default() += 1,
        }
    }
    let body_cells = |world: &spall_sim::world::SimWorld| {
        let mut counts: BTreeMap<MaterialId, u64> = BTreeMap::new();
        for body in world.bodies().filter(|b| b.entity.is_some()) {
            for coord in body.volume.resident_brick_coords() {
                for index in 0..32_768u32 {
                    if let Ok(Sample::Filled(m)) = body.volume.sample_local(coord, index) {
                        *counts.entry(m).or_default() += 1;
                    }
                }
            }
        }
        counts
    };
    let held = body_cells(sim.world());
    for (material, count) in &moved {
        assert!(
            held.get(material).copied().unwrap_or(0) >= *count,
            "{material:?}: {count} cells left the terrain but the bodies hold {:?}",
            held.get(material)
        );
    }
    println!(
        "{} cells moved into bodies across {} materials",
        moved.values().sum::<u64>(),
        moved.len()
    );
    assert!(
        moved.keys().any(|m| m.0 >= VARIANT_ID_BASE),
        "tinted cells moved with the split"
    );

    // Save, then restore from the database: the same variants come back.
    let dir = std::env::temp_dir().join(format!("spall_appearance_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("world.db");
    let cfg = spall_server::persist::PersistConfig {
        world_id: 0xA99E_0000_0000_0095,
        seed: 1,
        generator_version: 1,
    };
    let checkpoint = spall_server::persist::capture(&sim, &cfg, 0).expect("capture");
    spall_store::Writer::open(&db)
        .unwrap()
        .publish_checkpoint(&checkpoint)
        .unwrap();
    let recovery = spall_store::recover(&db).unwrap();
    let (restored, _) = spall_server::persist::restore(
        &recovery,
        &cfg,
        spall_server::persist::RecoveryChoice::RequireClean,
        game::manifest(),
        spall_structure::AnchorPlane::at(0),
        spall_physics::PhysicsConfig::default(),
    )
    .expect("restore under the current manifest");
    for (cell, before) in &kept {
        if let Sample::Filled(now) = material_at(&sim, *cell) {
            assert_eq!(now, *before);
            assert_eq!(
                material_at(&restored, *cell),
                Sample::Filled(now),
                "{cell:?} changed across save/restore"
            );
        }
    }
    assert_eq!(
        body_cells(restored.world()),
        held,
        "detached bodies keep every variant across save/restore"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
