//! The pack (Tab): the player kneels and unrolls a tarp on the ground in
//! front of them, and the inventory lies on it as the game's item models.
//! The world keeps running; the tarp and items are drawn client-side only,
//! and the server's inventory (a list of stable item IDs) is fetched over the
//! progression stream while the pack is open.

use super::*;
use glam::Quat;
use spall_protocol::{InventoryEntry, ProgressionOperation, ProgressionRequest};

/// Seconds to unroll the tarp (and kneel), or to roll it back up.
const UNROLL_S: f32 = 0.45;
/// Tarp size: across the player's view (X) and away from them (Z).
const TARP_W_M: f32 = 1.3;
const TARP_D_M: f32 = 0.9;
const TARP_THICK_M: f32 = 0.012;
/// From the player's feet to the tarp's near edge.
const TARP_NEAR_M: f32 = 0.3;
/// How far the eye drops and leans forward when kneeling.
const KNEEL_DROP_M: f32 = 0.75;
const KNEEL_LEAN_M: f32 = 0.15;
/// Item stacks laid out per row, and rows that fit on the tarp.
const COLUMNS: usize = 4;
const ROWS: usize = 3;
/// A stack shows up to this many copies of its model; the label has the count.
const MAX_COPIES: u32 = 4;
/// How often an open pack asks the server for the inventory again.
const REFRESH: Duration = Duration::from_millis(1500);
/// A hovered item lifts this far off the tarp.
const HOVER_LIFT_M: f32 = 0.04;
/// The cursor picks the nearest item within this many logical pixels.
const HOVER_RADIUS_PX: f32 = 70.0;

/// Where the pack was opened: the player's feet and facing.
#[derive(Debug, Clone, Copy)]
struct Anchor {
    feet: Vec3,
    yaw: f32,
}

/// A label to draw over one item stack (or the empty tarp).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct PackLabel {
    pub(super) item: Option<u16>,
    pub(super) at: Vec3,
    pub(super) text: String,
}

/// A label projected to the screen, in logical pixels.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct PackLabelView {
    pub(super) at: [f32; 2],
    pub(super) text: String,
    pub(super) hovered: bool,
}

pub(super) struct Pack {
    open: bool,
    /// 0 rolled up and standing, 1 unrolled and kneeling (linear in time).
    amount: f32,
    anchor: Option<Anchor>,
    /// `None` until the first answer since the pack was opened.
    inventory: Option<Vec<InventoryEntry>>,
    requested_at: Option<Instant>,
    next_request_id: u64,
    pub(super) hovered: Option<u16>,
}

impl Pack {
    pub(super) fn new() -> Self {
        // The server keeps a per-player high-water mark of request IDs, so
        // start each session above any earlier one: wall-clock milliseconds.
        let next_request_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1, |since| since.as_millis() as u64)
            .max(1);
        Self {
            open: false,
            amount: 0.0,
            anchor: None,
            inventory: None,
            requested_at: None,
            next_request_id,
            hovered: None,
        }
    }

    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    /// Open (kneeling) or still rolling up: the camera and tarp are in play.
    pub(super) fn is_shown(&self) -> bool {
        self.open || self.amount > 0.0
    }

    pub(super) fn open(&mut self, feet: [f64; 3], yaw: f32) {
        if !self.is_shown() {
            self.anchor = Some(Anchor {
                feet: Vec3::new(feet[0] as f32, feet[1] as f32, feet[2] as f32),
                yaw,
            });
        }
        self.open = true;
        self.inventory = None;
        self.requested_at = None;
    }

    pub(super) fn close(&mut self) {
        self.open = false;
        self.hovered = None;
    }

    /// Steps the unroll/kneel animation by `dt_s`.
    pub(super) fn advance(&mut self, dt_s: f32) {
        let step = dt_s.max(0.0) / UNROLL_S;
        self.amount = if self.open {
            (self.amount + step).min(1.0)
        } else {
            (self.amount - step).max(0.0)
        };
        if self.amount == 0.0 && !self.open {
            self.anchor = None;
        }
    }

    /// Eased animation amount.
    fn eased(&self) -> f32 {
        let t = self.amount;
        t * t * (3.0 - 2.0 * t)
    }

    /// Takes the newest progression answer, and returns the inventory
    /// request to send when one is due.
    pub(super) fn sync_inventory(
        &mut self,
        reply: Option<spall_protocol::ProgressionResponse>,
        catalog_version: u32,
        now: Instant,
    ) -> Option<ProgressionRequest> {
        if !self.open {
            return None;
        }
        if let Some(reply) = reply {
            self.inventory = Some(reply.inventory);
        }
        if self
            .requested_at
            .is_some_and(|at| now.saturating_duration_since(at) < REFRESH)
        {
            return None;
        }
        self.requested_at = Some(now);
        let request_id = self.next_request_id;
        self.next_request_id += 1;
        Some(ProgressionRequest {
            request_id,
            catalog_version,
            expected_inventory_revision: 0,
            operation: ProgressionOperation::InspectInventory,
        })
    }

    /// The kneeling eye and the look toward the tarp, blended in as the pack
    /// opens; the player's own look is untouched and comes back on close.
    pub(super) fn camera(&self, eye: Vec3, look: Vec3) -> (Vec3, Vec3) {
        let Some(anchor) = self.anchor else {
            return (eye, look);
        };
        let s = self.eased();
        let forward = forward(anchor.yaw);
        let kneel = eye - Vec3::Y * (KNEEL_DROP_M * s) + forward * (KNEEL_LEAN_M * s);
        let target = tarp_centre(anchor) + Vec3::Y * 0.05;
        let to_tarp = (target - kneel).normalize_or(look);
        let blended = (look * (1.0 - s) + to_tarp * s).normalize_or(look);
        (kneel, blended)
    }

    /// The roll position along the tarp: local Z of the unrolled edge.
    fn roll_z(&self) -> f32 {
        TARP_D_M * 0.5 - TARP_D_M * self.eased()
    }

    /// The tarp, its roll, the corner stones and the laid-out items.
    pub(super) fn instances(&self, view: &dyn PackView) -> Vec<CubeInstance> {
        let Some(anchor) = self.anchor else {
            return Vec::new();
        };
        let rotation = Quat::from_rotation_y(-anchor.yaw);
        let origin = tarp_centre(anchor);
        let place = |local: Vec3, local_rotation: Quat, material: u32, size: [f32; 3]| {
            CubeInstance::new(
                (origin + rotation * local).to_array(),
                material,
                size,
                (rotation * local_rotation).to_array(),
            )
        };
        let tarp = view.tarp_material();
        let [twine, stone] = view.trim_materials();
        let near = TARP_D_M * 0.5;
        let roll_z = self.roll_z();
        let mut out = Vec::new();

        let laid = near - roll_z;
        if laid > 0.002 {
            out.push(place(
                Vec3::new(0.0, TARP_THICK_M * 0.5, (near + roll_z) * 0.5),
                Quat::IDENTITY,
                tarp,
                [TARP_W_M, TARP_THICK_M, laid],
            ));
        }
        // The roll still to unroll, tied with two turns of twine.
        let radius = 0.03 + 0.05 * (1.0 - self.eased());
        let roll_centre = Vec3::new(0.0, radius, roll_z - radius * 0.5);
        out.push(place(
            roll_centre,
            Quat::IDENTITY,
            tarp,
            [TARP_W_M, radius * 2.0, radius * 2.0],
        ));
        out.push(place(
            roll_centre,
            Quat::from_rotation_x(std::f32::consts::FRAC_PI_4),
            tarp,
            [TARP_W_M - 0.02, radius * 2.0, radius * 2.0],
        ));
        if self.eased() < 0.95 {
            for x in [-0.4, 0.4] {
                out.push(place(
                    roll_centre + Vec3::X * x,
                    Quat::IDENTITY,
                    twine,
                    [0.02, radius * 2.0 + 0.01, radius * 2.0 + 0.01],
                ));
            }
        }
        // Stones pin the near corners, and the far ones once they are down.
        let corners = [
            (Vec3::new(-TARP_W_M * 0.5, 0.0, near), true),
            (Vec3::new(TARP_W_M * 0.5, 0.0, near), true),
            (
                Vec3::new(-TARP_W_M * 0.5, 0.0, -near),
                roll_z <= -near + 0.01,
            ),
            (
                Vec3::new(TARP_W_M * 0.5, 0.0, -near),
                roll_z <= -near + 0.01,
            ),
        ];
        for (index, (corner, down)) in corners.into_iter().enumerate() {
            if down {
                let size = 0.07 + 0.01 * index as f32;
                out.push(place(
                    corner + Vec3::Y * (size * 0.4),
                    Quat::from_rotation_y(0.4 * index as f32),
                    stone,
                    [size, size * 0.8, size * 0.9],
                ));
            }
        }

        for (slot, entry) in self.laid_out().into_iter().enumerate() {
            let centre = slot_centre(slot);
            // Items appear as the tarp unrolls under them.
            if centre.z < roll_z {
                continue;
            }
            let lift = if self.hovered == Some(entry.item_id) {
                HOVER_LIFT_M
            } else {
                0.0
            };
            let model = view.item_model(entry.item_id);
            let height = model_height(model);
            for copy in 0..entry.count.min(MAX_COPIES) {
                let jitter = jitter(entry.item_id, copy);
                let copy_rotation = Quat::from_rotation_y(jitter.z);
                let base = centre
                    + Vec3::new(
                        jitter.x,
                        TARP_THICK_M + lift + height * copy as f32,
                        jitter.y,
                    );
                if model.is_empty() {
                    // An item this build has no model for: a plain crate.
                    out.push(place(
                        base + Vec3::Y * 0.05,
                        copy_rotation,
                        twine,
                        [0.1, 0.1, 0.1],
                    ));
                }
                for cube in model {
                    let local = base + copy_rotation * Vec3::from_array(cube.offset);
                    out.push(place(
                        local,
                        copy_rotation * Quat::from_array(cube.rotation),
                        cube.material,
                        cube.size,
                    ));
                }
            }
        }
        out
    }

    /// The stacks that fit on the tarp, in slot order.
    fn laid_out(&self) -> Vec<InventoryEntry> {
        self.inventory
            .as_deref()
            .unwrap_or_default()
            .iter()
            .take(COLUMNS * ROWS)
            .copied()
            .collect()
    }

    /// One label per laid-out stack, above it in world space; a note on the
    /// tarp when the pack is empty or more stacks exist than fit.
    pub(super) fn labels(&self, view: &dyn PackView) -> Vec<PackLabel> {
        let Some(anchor) = self.anchor.filter(|_| self.open && self.amount >= 1.0) else {
            return Vec::new();
        };
        let Some(inventory) = &self.inventory else {
            return Vec::new();
        };
        let rotation = Quat::from_rotation_y(-anchor.yaw);
        let origin = tarp_centre(anchor);
        let world = |local: Vec3| origin + rotation * local;
        if inventory.is_empty() {
            return vec![PackLabel {
                item: None,
                at: world(Vec3::Y * 0.05),
                text: "Nothing in the pack".to_owned(),
            }];
        }
        let mut labels: Vec<_> = self
            .laid_out()
            .into_iter()
            .enumerate()
            .map(|(slot, entry)| {
                let height = model_height(view.item_model(entry.item_id)).max(0.1);
                let shown = entry.count.min(MAX_COPIES) as f32;
                let name = view.item_name(entry.item_id).unwrap_or("Unknown item");
                PackLabel {
                    item: Some(entry.item_id),
                    at: world(slot_centre(slot) + Vec3::Y * (height * shown + 0.06)),
                    text: if entry.count == 1 {
                        name.to_owned()
                    } else {
                        format!("{name} \u{d7}{}", entry.count)
                    },
                }
            })
            .collect();
        let hidden = inventory.len().saturating_sub(COLUMNS * ROWS);
        if hidden > 0 {
            labels.push(PackLabel {
                item: None,
                at: world(Vec3::new(0.0, 0.02, TARP_D_M * 0.5 + 0.05)),
                text: format!("+{hidden} more"),
            });
        }
        labels
    }
}

/// Horizontal facing for a yaw (zero looks along `-Z`).
fn forward(yaw: f32) -> Vec3 {
    Vec3::new(yaw.sin(), 0.0, -yaw.cos())
}

/// The tarp's centre on the ground, in world metres.
fn tarp_centre(anchor: Anchor) -> Vec3 {
    anchor.feet + forward(anchor.yaw) * (TARP_NEAR_M + TARP_D_M * 0.5)
}

/// Tarp-local centre of a slot: rows fill from the far edge toward the
/// player, left to right.
fn slot_centre(slot: usize) -> Vec3 {
    let column = slot % COLUMNS;
    let row = slot / COLUMNS;
    let pitch_x = TARP_W_M / COLUMNS as f32;
    let pitch_z = TARP_D_M / ROWS as f32;
    Vec3::new(
        -TARP_W_M * 0.5 + pitch_x * (column as f32 + 0.5),
        0.0,
        -TARP_D_M * 0.5 + pitch_z * (row as f32 + 0.5),
    )
}

/// Height of a model resting on `y = 0`.
fn model_height(model: &[CubeInstance]) -> f32 {
    model
        .iter()
        .map(|cube| cube.offset[1] + cube.size[1] * 0.5)
        .fold(0.0, f32::max)
}

/// A small, stable scatter for copy `copy` of an item: X and Z offsets in
/// metres, and a yaw in radians in `z`. The first copy sits square.
fn jitter(item: u16, copy: u32) -> Vec3 {
    if copy == 0 {
        return Vec3::ZERO;
    }
    let hash = |salt: u32| {
        let mut h = u32::from(item)
            .wrapping_mul(0x9E37_79B9)
            .wrapping_add(copy.wrapping_mul(0x85EB_CA6B))
            .wrapping_add(salt.wrapping_mul(0xC2B2_AE35));
        h ^= h >> 15;
        h = h.wrapping_mul(0x2C1B_3C6D);
        h ^= h >> 12;
        (h & 0xFFFF) as f32 / 65535.0 * 2.0 - 1.0
    };
    Vec3::new(hash(1) * 0.025, hash(2) * 0.025, hash(3) * 0.35)
}

/// The label nearest the cursor within reach, if any.
pub(super) fn hovered_item(labels: &[(Option<u16>, [f32; 2])], cursor: [f32; 2]) -> Option<u16> {
    labels
        .iter()
        .filter_map(|&(item, at)| {
            let item = item?;
            let distance = ((at[0] - cursor[0]).powi(2) + (at[1] - cursor[1]).powi(2)).sqrt();
            (distance <= HOVER_RADIUS_PX).then_some((item, distance))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(item, _)| item)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct TestView(Vec<CubeInstance>);

    impl PackView for TestView {
        fn catalog_version(&self) -> u32 {
            1
        }
        fn item_name(&self, item_id: u16) -> Option<&str> {
            (item_id == 1).then_some("Log")
        }
        fn item_model(&self, item_id: u16) -> &[CubeInstance] {
            if item_id == 1 { &self.0 } else { &[] }
        }
        fn tarp_material(&self) -> u32 {
            7
        }
        fn trim_materials(&self) -> [u32; 2] {
            [8, 9]
        }
    }

    fn log_view() -> TestView {
        TestView(vec![CubeInstance::new(
            [0.0, 0.05, 0.0],
            3,
            [0.4, 0.1, 0.1],
            CubeInstance::IDENTITY_ROTATION,
        )])
    }

    fn opened(inventory: Vec<InventoryEntry>) -> Pack {
        let mut pack = Pack::new();
        pack.open([10.0, 5.0, -3.0], 0.0);
        pack.inventory = Some(inventory);
        pack.advance(UNROLL_S);
        pack
    }

    #[test]
    fn opening_kneels_toward_the_tarp_and_closing_gives_the_view_back() {
        let mut pack = Pack::new();
        let eye = Vec3::new(10.0, 6.6, -3.0);
        let ahead = Vec3::new(0.0, 0.0, -1.0);
        assert_eq!(pack.camera(eye, ahead), (eye, ahead));

        pack.open([10.0, 5.0, -3.0], 0.0);
        pack.advance(UNROLL_S * 2.0);
        let (kneel, look) = pack.camera(eye, ahead);
        assert!((kneel.y - (eye.y - KNEEL_DROP_M)).abs() < 1e-4);
        assert!(look.y < -0.5, "looks down at the tarp: {look:?}");
        assert!(look.z < 0.0, "still facing the way the player faced");

        pack.close();
        assert!(pack.is_shown(), "rolls up over time, not at once");
        pack.advance(UNROLL_S);
        assert!(!pack.is_shown());
        assert_eq!(pack.camera(eye, ahead), (eye, ahead));
        assert!(pack.instances(&log_view()).is_empty());
    }

    #[test]
    fn items_lie_on_the_tarp_in_front_of_the_player_whatever_their_facing() {
        for yaw in [0.0_f32, 1.0, -2.5] {
            let mut pack = Pack::new();
            pack.open([0.0, 2.0, 0.0], yaw);
            pack.inventory = Some(vec![InventoryEntry {
                item_id: 1,
                count: 2,
            }]);
            pack.advance(UNROLL_S);
            let view = log_view();
            let cubes = pack.instances(&view);
            let logs: Vec<_> = cubes.iter().filter(|c| c.material == 3).collect();
            assert_eq!(logs.len(), 2, "two copies of a stack of two");
            for log in logs {
                let at = Vec3::from_array(log.offset);
                let ahead = at.dot(forward(yaw));
                assert!(
                    (TARP_NEAR_M..TARP_NEAR_M + TARP_D_M).contains(&ahead),
                    "yaw {yaw}: {ahead} m ahead"
                );
                assert!(at.y > 2.0 && at.y < 2.4, "on the tarp: {at:?}");
            }
        }
    }

    #[test]
    fn half_unrolled_tarp_shows_only_the_items_it_has_reached() {
        let mut pack = Pack::new();
        pack.open([0.0; 3], 0.0);
        pack.inventory = Some(
            (1..=8)
                .map(|item_id| InventoryEntry { item_id, count: 1 })
                .collect(),
        );
        let view = log_view();
        // Nothing unrolled: only the roll and the near stones.
        let crates = |pack: &Pack| {
            pack.instances(&view)
                .iter()
                .filter(|c| c.size == [0.1, 0.1, 0.1])
                .count()
        };
        assert_eq!(crates(&pack), 0);
        pack.advance(UNROLL_S * 0.5);
        let part = crates(&pack);
        pack.advance(UNROLL_S);
        // Item 1 is a log; items 2..=8 have no model and show as crates.
        assert_eq!(crates(&pack), 7);
        assert!(part > 0 && part < 7, "{part} crates half way");
    }

    #[test]
    fn labels_name_and_count_each_stack_and_note_an_empty_pack() {
        let view = log_view();
        let pack = opened(vec![
            InventoryEntry {
                item_id: 1,
                count: 12,
            },
            InventoryEntry {
                item_id: 4,
                count: 1,
            },
        ]);
        let texts: Vec<_> = pack.labels(&view).into_iter().map(|l| l.text).collect();
        assert_eq!(texts, ["Log \u{d7}12", "Unknown item"]);

        let empty = opened(Vec::new());
        let labels = empty.labels(&view);
        assert_eq!(labels.len(), 1);
        assert_eq!(labels[0].item, None);

        let many = opened(
            (1..=15)
                .map(|item_id| InventoryEntry { item_id, count: 1 })
                .collect(),
        );
        let labels = many.labels(&view);
        assert_eq!(labels.len(), COLUMNS * ROWS + 1);
        assert_eq!(labels.last().unwrap().text, "+3 more");
    }

    #[test]
    fn open_pack_asks_for_the_inventory_and_keeps_the_newest_answer() {
        let mut pack = Pack::new();
        let now = Instant::now();
        assert_eq!(pack.sync_inventory(None, 1, now), None, "closed");

        pack.open([0.0; 3], 0.0);
        let first = pack.sync_inventory(None, 1, now).expect("asks at open");
        assert_eq!(first.operation, ProgressionOperation::InspectInventory);
        assert_eq!(first.catalog_version, 1);
        assert_eq!(
            pack.sync_inventory(None, 1, now + Duration::from_millis(100)),
            None
        );
        let second = pack
            .sync_inventory(None, 1, now + REFRESH)
            .expect("refresh");
        assert!(second.request_id > first.request_id);

        let reply = spall_protocol::ProgressionResponse {
            request_id: second.request_id,
            catalog_version: 1,
            inventory_revision: 3,
            outcome: spall_protocol::ProgressionOutcome::Inventory,
            inventory: vec![InventoryEntry {
                item_id: 1,
                count: 5,
            }],
        };
        pack.sync_inventory(Some(reply), 1, now + REFRESH);
        assert_eq!(pack.laid_out().len(), 1);
    }

    #[test]
    fn hover_picks_the_nearest_item_within_reach() {
        let labels = [
            (Some(1), [100.0, 100.0]),
            (Some(2), [160.0, 100.0]),
            (None, [130.0, 100.0]),
        ];
        assert_eq!(hovered_item(&labels, [120.0, 100.0]), Some(1));
        assert_eq!(hovered_item(&labels, [150.0, 110.0]), Some(2));
        assert_eq!(hovered_item(&labels, [400.0, 400.0]), None);
    }
}
