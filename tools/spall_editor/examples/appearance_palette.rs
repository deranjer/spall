//! Extends the sandbox's appearance-variant palette from authored SPVX assets.
//!
//! The runtime stores one `MaterialId` per cell, so an authored tint survives
//! destruction, replication and reload only if it *is* a material. This tool
//! gathers every authored tint per portable material key from the repository's
//! asset fixtures and the engine's built-in assets and median-cuts each key's
//! colours down to at most `VARIANTS_PER_KEY` candidates.
//!
//! **It only appends.** The frozen v3 variants (`appearance_v3.rs`) and every row
//! already in `appearance_extensions.rs` keep their id, name and colour. A
//! candidate closer than `REUSE_DISTANCE` to an existing variant of the same key
//! maps to it and adds nothing; any other candidate becomes a new row with the
//! next free id (at least `FIRST_EXTENSION_ID`, above every existing id), named
//! `key.vNN` after the key's variant count. Output is committed; run this only
//! when the assets change:
//!
//! `cargo run -p spall_editor --example appearance_palette`
//!
//! Prints, per key, what was kept, reused and added, and the resulting
//! quantisation error. Deterministic for the same assets and existing rows.
//! Recolouring or renumbering an existing variant is deliberately not possible
//! here: it changes what saved worlds mean and needs a versioned manifest
//! migration in `examples/sandbox/src/game.rs`.
use std::collections::BTreeMap;
use std::fmt::Write as _;

use spall_editor::{EditorModel, VoxelAssetFile};

/// Palette entries per material key (an authored asset palette can hold 4096).
const VARIANTS_PER_KEY: usize = 12;
/// Only these portable keys have game materials to vary.
const KEYS: [&str; 6] = [
    "grass",
    "foliage.oak",
    "wood.oak",
    "stone.granite",
    "sandstone",
    "dirt",
];
const FROZEN: &str = "examples/sandbox/src/appearance_v3.rs";
const OUTPUT: &str = "examples/sandbox/src/appearance_extensions.rs";
/// Keep in step with `sandbox::appearance::EXTENSION_ID_BASE` (a sandbox test
/// asserts they agree).
const FIRST_EXTENSION_ID: u16 = 256;
/// A candidate this close (redmean sRGB distance) to an existing variant of its
/// key reuses it instead of adding a row.
const REUSE_DISTANCE: f64 = 6.0;
const PROJECTS: [&str; 4] = [
    "fixtures/terrain-trees",
    "fixtures/terrain-trees-v2",
    "fixtures/terrain-trees-forest",
    "fixtures/lighting-room",
];

type Rgb = [u8; 3];

fn collect(asset: &VoxelAssetFile, out: &mut BTreeMap<String, BTreeMap<Rgb, u64>>) {
    for (cell, material) in &asset.voxels {
        let (Some(key), Some(color)) = (asset.material_keys.get(material), asset.colors.get(cell))
        else {
            continue;
        };
        if KEYS.contains(&key.as_str()) {
            *out.entry(key.clone())
                .or_default()
                .entry(*color)
                .or_default() += 1;
        }
    }
}

/// Median cut: split the box with the most weight along its widest channel at
/// the weighted median until there are `k` boxes; each box becomes its
/// weighted-mean colour.
fn median_cut(histogram: &BTreeMap<Rgb, u64>, k: usize) -> Vec<Rgb> {
    let mut boxes: Vec<Vec<(Rgb, u64)>> = vec![histogram.iter().map(|(c, n)| (*c, *n)).collect()];
    while boxes.len() < k {
        let Some(index) = (0..boxes.len())
            .filter(|i| boxes[*i].len() > 1)
            .max_by_key(|i| boxes[*i].iter().map(|(_, n)| n).sum::<u64>())
        else {
            break;
        };
        let mut entries = boxes.swap_remove(index);
        let range = |channel: usize| {
            let (lo, hi) = entries.iter().fold((255u8, 0u8), |(lo, hi), (c, _)| {
                (lo.min(c[channel]), hi.max(c[channel]))
            });
            hi - lo
        };
        let channel = (0..3).max_by_key(|c| range(*c)).unwrap();
        entries.sort_by_key(|(c, _)| (c[channel], *c));
        let total: u64 = entries.iter().map(|(_, n)| n).sum();
        let mut seen = 0;
        let mut split = entries.len() / 2;
        for (i, (_, n)) in entries.iter().enumerate() {
            seen += n;
            if seen * 2 >= total {
                split = (i + 1).min(entries.len() - 1);
                break;
            }
        }
        let upper = entries.split_off(split);
        boxes.push(entries);
        boxes.push(upper);
    }
    let mut palette: Vec<Rgb> = boxes
        .iter()
        .map(|entries| {
            let total: u64 = entries.iter().map(|(_, n)| n).sum();
            std::array::from_fn(|c| {
                let sum: u64 = entries
                    .iter()
                    .map(|(color, n)| u64::from(color[c]) * n)
                    .sum();
                ((sum + total / 2) / total) as u8
            })
        })
        .collect();
    palette.sort_by_key(|c| {
        (
            u32::from(c[0]) * 299 + u32::from(c[1]) * 587 + u32::from(c[2]) * 114,
            *c,
        )
    });
    palette.dedup();
    palette
}

fn distance(a: Rgb, b: Rgb) -> f64 {
    // "Redmean" weighted sRGB distance: cheap and close to perceptual.
    let r = (f64::from(a[0]) + f64::from(b[0])) / 2.0;
    let (dr, dg, db) = (
        f64::from(a[0]) - f64::from(b[0]),
        f64::from(a[1]) - f64::from(b[1]),
        f64::from(a[2]) - f64::from(b[2]),
    );
    ((2.0 + r / 256.0) * dr * dr + 4.0 * dg * dg + (2.0 + (255.0 - r) / 256.0) * db * db).sqrt()
}

/// One existing row: `(id, key, name, srgb)`.
type Row = (u16, String, String, Rgb);

/// Parses `(id, "key", "name", [r, g, b]),` lines of a generated table.
fn parse_rows(path: &str) -> Result<Vec<Row>, Box<dyn std::error::Error>> {
    let text = std::fs::read_to_string(path)?;
    let mut rows = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let Some(body) = line.strip_prefix('(').and_then(|l| l.strip_suffix("),")) else {
            continue;
        };
        let Some((id, rest)) = body.split_once(", \"") else {
            continue;
        };
        let (key, rest) = rest.split_once("\", \"").ok_or("bad row")?;
        let (name, rest) = rest.split_once("\", [").ok_or("bad row")?;
        let colour: Vec<u8> = rest
            .trim_end_matches(']')
            .split(',')
            .map(|c| c.trim().parse())
            .collect::<Result<_, _>>()?;
        rows.push((
            id.trim().parse()?,
            key.to_owned(),
            name.to_owned(),
            [colour[0], colour[1], colour[2]],
        ));
    }
    Ok(rows)
}

/// Appends a row for every candidate of `key` that is not within
/// `REUSE_DISTANCE` of an existing variant of `key` (frozen or already added).
/// Never touches an existing row. Returns `(reused, added)`.
fn add_candidates(
    frozen: &[Row],
    rows: &mut Vec<Row>,
    key: &str,
    candidates: &[Rgb],
) -> (usize, usize) {
    let (mut reused, mut added) = (0, 0);
    for candidate in candidates {
        let all: Vec<&Row> = frozen.iter().chain(rows.iter()).collect();
        let nearest = all
            .iter()
            .filter(|row| row.1 == key)
            .map(|row| distance(*candidate, row.3))
            .fold(f64::MAX, f64::min);
        if nearest <= REUSE_DISTANCE {
            reused += 1;
            continue;
        }
        let id = all
            .iter()
            .map(|row| row.0 + 1)
            .max()
            .unwrap_or(FIRST_EXTENSION_ID)
            .max(FIRST_EXTENSION_ID);
        let count = all.iter().filter(|row| row.1 == key).count();
        rows.push((id, key.to_owned(), format!("{key}.v{count:02}"), *candidate));
        added += 1;
    }
    (reused, added)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let frozen = parse_rows(FROZEN)?;
    let mut rows = parse_rows(OUTPUT)?;
    let kept = rows.len();
    let existing =
        |rows: &[Row]| -> Vec<Row> { frozen.iter().chain(rows.iter()).cloned().collect() };
    let mut histograms: BTreeMap<String, BTreeMap<Rgb, u64>> = BTreeMap::new();
    for project in PROJECTS {
        let Ok(model) = EditorModel::load(project) else {
            eprintln!("skipping {project}: not found");
            continue;
        };
        for asset in model.voxel_assets.values() {
            collect(asset, &mut histograms);
        }
    }
    for bundled in spall_voxel::builtin_assets::builtin_voxel_assets() {
        let preview = EditorModel::new(
            std::env::temp_dir().join("spall-palette-scratch"),
            "scratch",
        )
        .preview_builtin_spvox(bundled.bytes)?;
        collect(&preview, &mut histograms);
    }

    for key in KEYS {
        let Some(histogram) = histograms.get(key) else {
            eprintln!("{key}: no authored tints found");
            continue;
        };
        let (reused, added) = add_candidates(
            &frozen,
            &mut rows,
            key,
            &median_cut(histogram, VARIANTS_PER_KEY),
        );
        // Quantisation error against every variant of the key, old and new.
        let all = existing(&rows);
        let palette: Vec<Rgb> = all.iter().filter(|r| r.1 == key).map(|r| r.3).collect();
        let (mut sum, mut worst, mut cells) = (0.0, 0.0_f64, 0_u64);
        for (color, count) in histogram {
            let error = palette
                .iter()
                .map(|p| distance(*color, *p))
                .fold(f64::MAX, f64::min);
            sum += error * *count as f64;
            worst = worst.max(error);
            cells += count;
        }
        println!(
            "{key}: {} distinct authored colours over {cells} cells; {} existing variants, {reused} candidates reused, {added} added; mean error {:.2}, worst {:.2} (redmean sRGB distance)",
            histogram.len(),
            palette.len() - added,
            sum / cells as f64,
            worst
        );
    }

    let mut source = [
        "//! @generated by `cargo run -p spall_editor --example appearance_palette`.",
        "//! Append-only: the generator keeps every row and adds new ones; never renumber",
        "//! or recolour a row (that needs a versioned manifest migration).",
        "",
        "/// `(id, portable material key, name, authored sRGB tint)`; ids start at",
        "/// `appearance::EXTENSION_ID_BASE` and only grow.",
        "pub const EXTENSIONS: &[(u16, &str, &str, [u8; 3])] = &[",
        "",
    ]
    .join(
        "
",
    );
    source.pop();
    for (id, key, name, c) in &rows {
        writeln!(
            source,
            "    ({id}, {key:?}, {name:?}, [{}, {}, {}]),",
            c[0], c[1], c[2]
        )?;
    }
    source.push_str(
        "];
",
    );
    std::fs::write(OUTPUT, source)?;
    println!(
        "wrote {OUTPUT}: {kept} existing rows kept, {} added",
        rows.len() - kept
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frozen() -> Vec<Row> {
        vec![
            (100, "grass".into(), "grass.v00".into(), [60, 110, 40]),
            (101, "grass".into(), "grass.v01".into(), [90, 140, 60]),
            (136, "dirt".into(), "dirt.v00".into(), [104, 78, 52]),
        ]
    }

    #[test]
    fn extending_appends_under_new_ids_and_never_touches_existing_rows() {
        let frozen = frozen();
        let mut rows = Vec::new();
        // One candidate already covered, one new grass, one new key.
        let (reused, added) =
            add_candidates(&frozen, &mut rows, "grass", &[[61, 111, 41], [200, 30, 30]]);
        assert_eq!((reused, added), (1, 1));
        let (_, added) = add_candidates(&frozen, &mut rows, "wood.oak", &[[100, 73, 45]]);
        assert_eq!(added, 1);
        assert_eq!(
            rows,
            vec![
                (256, "grass".into(), "grass.v02".into(), [200, 30, 30]),
                (257, "wood.oak".into(), "wood.oak.v00".into(), [100, 73, 45]),
            ],
            "ids start at the extension base, above the lamp (200) and every frozen id"
        );
        // Re-running with the same candidates adds nothing (idempotent).
        let before = rows.clone();
        let (reused, added) =
            add_candidates(&frozen, &mut rows, "grass", &[[61, 111, 41], [200, 30, 30]]);
        assert_eq!((reused, added), (2, 0));
        assert_eq!(rows, before);
    }

    #[test]
    fn ids_only_grow_across_successive_extensions() {
        let frozen = frozen();
        let mut rows = Vec::new();
        add_candidates(&frozen, &mut rows, "grass", &[[200, 30, 30]]);
        let first = rows[0].0;
        add_candidates(&frozen, &mut rows, "grass", &[[30, 30, 200]]);
        assert!(rows[1].0 > first);
        assert_eq!(rows[0].0, first, "the earlier row is unchanged");
    }
}
