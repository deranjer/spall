//! Engine-owned portable voxel assets shipped with Spall.
//!
//! These immutable source bytes are suitable for deterministic terrain
//! generation and editor import. Editors should copy an asset into a project
//! before allowing edits; project AssetIds remain project-local.

/// One bundled, portable voxel asset. `bytes` contains canonical SPVX data.
#[derive(Debug, Clone, Copy)]
pub struct BuiltinVoxelAsset {
    /// Stable engine catalog key, independent of project AssetIds.
    pub key: &'static str,
    /// Human-readable asset name stored in SPVX metadata.
    pub name: &'static str,
    /// Canonical filename in the engine source tree.
    pub file_name: &'static str,
    /// Immutable canonical SPVX bytes.
    pub bytes: &'static [u8],
}

const PALM_TREE: &[u8] = include_bytes!("../assets/builtin/voxel/palm_tree.spvox");
const WEEPING_WILLOW: &[u8] = include_bytes!("../assets/builtin/voxel/weeping_willow.spvox");

/// Returns the immutable built-in voxel catalog in stable key order.
pub fn builtin_voxel_assets() -> &'static [BuiltinVoxelAsset] {
    static ASSETS: [BuiltinVoxelAsset; 2] = [
        BuiltinVoxelAsset {
            key: "spall.terrain.palm_tree",
            name: "Terrain Palm Tree",
            file_name: "palm_tree.spvox",
            bytes: PALM_TREE,
        },
        BuiltinVoxelAsset {
            key: "spall.terrain.weeping_willow",
            name: "Terrain Weeping Willow",
            file_name: "weeping_willow.spvox",
            bytes: WEEPING_WILLOW,
        },
    ];
    &ASSETS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_assets_are_nonempty_and_have_stable_distinct_keys() {
        let assets = builtin_voxel_assets();
        assert_eq!(assets.len(), 2);
        assert!(assets.iter().all(|asset| asset.bytes.starts_with(b"SPVX")));
        assert_ne!(assets[0].key, assets[1].key);
        assert_eq!(assets[0].file_name, "palm_tree.spvox");
        assert_eq!(assets[1].file_name, "weeping_willow.spvox");
    }
}
