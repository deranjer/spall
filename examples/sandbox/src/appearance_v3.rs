//! The appearance variants of the first shipped variant manifest
//! ([`crate::game::manifest_v3_variants`]), frozen.
//!
//! **Never edit this file.** These ids, names and colours are what worlds saved
//! and replicated under that manifest mean; the manifest hash pins them. Later
//! variants are appended in `appearance_extensions.rs` under new ids; recolouring
//! or remapping one of these needs an explicit versioned manifest migration.

/// `(id, portable material key, name, authored sRGB tint)`.
pub const V3: &[(u16, &str, &str, [u8; 3])] = &[
    (100, "grass", "grass.v00", [62, 112, 44]),
    (101, "grass", "grass.v01", [66, 116, 46]),
    (102, "grass", "grass.v02", [69, 119, 48]),
    (103, "grass", "grass.v03", [72, 122, 49]),
    (104, "grass", "grass.v04", [74, 124, 50]),
    (105, "grass", "grass.v05", [76, 126, 51]),
    (106, "grass", "grass.v06", [78, 128, 52]),
    (107, "grass", "grass.v07", [80, 130, 53]),
    (108, "grass", "grass.v08", [82, 132, 54]),
    (109, "grass", "grass.v09", [84, 134, 55]),
    (110, "grass", "grass.v10", [87, 137, 56]),
    (111, "grass", "grass.v11", [92, 142, 59]),
    (112, "foliage.oak", "foliage.oak.v00", [79, 120, 51]),
    (113, "foliage.oak", "foliage.oak.v01", [90, 128, 54]),
    (114, "foliage.oak", "foliage.oak.v02", [91, 129, 54]),
    (115, "foliage.oak", "foliage.oak.v03", [83, 133, 55]),
    (116, "foliage.oak", "foliage.oak.v04", [94, 133, 57]),
    (117, "foliage.oak", "foliage.oak.v05", [91, 141, 61]),
    (118, "foliage.oak", "foliage.oak.v06", [98, 138, 60]),
    (119, "foliage.oak", "foliage.oak.v07", [112, 146, 66]),
    (120, "foliage.oak", "foliage.oak.v08", [106, 156, 76]),
    (121, "foliage.oak", "foliage.oak.v09", [121, 155, 67]),
    (122, "foliage.oak", "foliage.oak.v10", [126, 156, 72]),
    (123, "foliage.oak", "foliage.oak.v11", [150, 174, 92]),
    (124, "wood.oak", "wood.oak.v00", [100, 73, 45]),
    (125, "wood.oak", "wood.oak.v01", [103, 75, 49]),
    (126, "wood.oak", "wood.oak.v02", [106, 78, 52]),
    (127, "wood.oak", "wood.oak.v03", [108, 80, 54]),
    (128, "wood.oak", "wood.oak.v04", [109, 81, 55]),
    (129, "wood.oak", "wood.oak.v05", [110, 82, 56]),
    (130, "wood.oak", "wood.oak.v06", [116, 84, 52]),
    (131, "wood.oak", "wood.oak.v07", [128, 98, 58]),
    (132, "wood.oak", "wood.oak.v08", [129, 99, 59]),
    (133, "wood.oak", "wood.oak.v09", [132, 102, 62]),
    (134, "wood.oak", "wood.oak.v10", [138, 108, 68]),
    (135, "wood.oak", "wood.oak.v11", [142, 112, 72]),
    (136, "dirt", "dirt.v00", [104, 78, 52]),
];
