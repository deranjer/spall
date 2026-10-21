//! Cross-check authored game assets with the independent engine decoder.

use spall_asset::{Asset, LoopMode, decode, sample_clip};

fn animals() -> [(Asset, &'static str, &'static str); 5] {
    [
        (
            decode(include_bytes!("../assets/animals/trout.spvox")).unwrap(),
            "swim",
            "tail_peduncle_and_fork",
        ),
        (
            decode(include_bytes!("../assets/animals/crow.spvox")).unwrap(),
            "flap",
            "wing_near",
        ),
        (
            decode(include_bytes!("../assets/animals/deer.spvox")).unwrap(),
            "walk",
            "foreleg_near",
        ),
        (
            decode(include_bytes!("../assets/animals/bear.spvox")).unwrap(),
            "walk",
            "foreleg_near",
        ),
        (
            decode(include_bytes!("../assets/animals/leopard.spvox")).unwrap(),
            "walk",
            "foreleg_near",
        ),
    ]
}

#[test]
fn authored_animals_decode_with_explicit_materials_and_portable_identity() {
    let assets = animals();
    let manifest = sandbox::game::manifest();
    let mut identities = std::collections::BTreeSet::new();
    for (asset, movement, _) in assets {
        assert!(identities.insert(asset.uuid));
        assert_eq!(asset.cell_size_code, 1, "animal detail must remain 6.25 cm");
        assert!(asset.parts.len() >= 5);
        assert!(
            asset
                .runs
                .iter()
                .map(|r| u64::from(r.length_x))
                .sum::<u64>()
                > 100
        );
        for key in &asset.material_keys {
            assert!(
                manifest.entries().iter().any(|m| &m.name == key),
                "unmapped {key}"
            );
        }
        for name in ["idle", movement] {
            let clip = asset.clips.iter().find(|c| c.name == name).unwrap();
            assert_eq!(clip.loop_mode, LoopMode::Loop);
            assert!(clip.duration_ticks > 0);
            assert!(!clip.tracks.is_empty());
        }
        assert!(asset.parts.iter().all(|p| p.rest.scale == glam::Vec3::ONE));
    }
}

#[test]
fn authored_movement_changes_the_intended_joint_and_wraps_continuously() {
    for (asset, movement, joint_name) in animals() {
        let joint = asset
            .parts
            .iter()
            .find(|p| p.name == joint_name)
            .unwrap_or_else(|| panic!("{} lacks expected joint {joint_name}", asset.name));
        let duration = f64::from(
            asset
                .clips
                .iter()
                .find(|c| c.name == movement)
                .unwrap()
                .duration_ticks,
        );
        let start = sample_clip(&asset, movement, 0.0).unwrap();
        let quarter = sample_clip(&asset, movement, duration * 0.25).unwrap();
        let endpoint = sample_clip(&asset, movement, duration).unwrap();
        assert_ne!(
            start[&joint.id].transform.rotation,
            quarter[&joint.id].transform.rotation
        );
        assert_eq!(start, endpoint);
        for time in [0.0, duration * 0.125, duration * 0.5, duration * 0.875] {
            for pose in sample_clip(&asset, movement, time).unwrap().values() {
                assert!(pose.transform.translation.is_finite());
                assert!(pose.transform.rotation.is_finite());
                assert!((pose.transform.rotation.length() - 1.0).abs() < 0.0001);
                assert_eq!(pose.transform.scale, glam::Vec3::ONE);
            }
        }
    }
}
