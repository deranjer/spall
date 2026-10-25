//! Log of terrain collider changes, used to tell when a collision window is still valid.

use std::collections::VecDeque;

/// A terrain collider change: the revision it produced and the world box (min, max, metres) it
/// touched, `None` meaning anywhere.
pub(super) type TerrainChange = (u64, Option<([f64; 3], [f64; 3])>);

/// Most terrain collider changes remembered for window invalidation.
pub(super) const MAX_TERRAIN_CHANGES: usize = 4096;

/// Whether every terrain collider change after `built_revision` up to `current_revision` is
/// recorded in `changes` and lies outside the collision window centred at `centre` (world
/// metres). A change that is unrecorded, or recorded as "anywhere", or older than the log
/// reaches, answers `false`: the window must be rebuilt.
pub(super) fn changes_miss_window(
    changes: &VecDeque<TerrainChange>,
    floor: u64,
    built_revision: u64,
    current_revision: u64,
    centre: [f64; 3],
) -> bool {
    if built_revision < floor || built_revision > current_revision {
        return false;
    }
    let reach = f64::from(spall_physics::WINDOW_RADIUS_M) + 1.0;
    let mut seen = 0u64;
    for (revision, region) in changes.iter().filter(|(r, _)| *r > built_revision) {
        let Some((lo, hi)) = region else {
            return false;
        };
        let overlaps = (0..3).all(|a| lo[a] <= centre[a] + reach && hi[a] >= centre[a] - reach);
        if overlaps {
            return false;
        }
        debug_assert!(*revision <= current_revision);
        seen += 1;
    }
    // Every revision in between must have been recorded; a bump made elsewhere is unknown.
    seen == current_revision - built_revision
}
