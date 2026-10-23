//! The pack (Tab): the player kneels and unrolls a tarp on the ground in
//! front of them, and the inventory lies on it as the game's item models.
//! The world keeps running; the tarp and items are drawn client-side only,
//! and the server's inventory (a list of stable item IDs) is fetched over the
//! progression stream while the pack is open.

use super::*;
use glam::Quat;
use spall_protocol::{InventoryEntry, ProgressionOperation, ProgressionRequest, SiteEntry};

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
/// Pegs around the camp's boundary while the pack is open.
const RING_PEGS: usize = 48;

/// Where the pack was opened: the player's feet and facing.
#[derive(Debug, Clone, Copy)]
struct Anchor {
    feet: Vec3,
    yaw: f32,
}

/// A label to draw over one item stack (or the empty tarp).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct PackLabel {
    pub(super) target: Option<PackTarget>,
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
    pub(super) hovered: Option<PackTarget>,
    /// The player's placed structures, from the newest progression answer.
    sites: Vec<SiteEntry>,
    /// Ground points of the pegs around the camp (see [`Pack::set_ring`]).
    ring: Vec<Vec3>,
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
            sites: Vec::new(),
            ring: Vec::new(),
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
            self.take_sites(reply.sites);
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
        // Kneeling, the eye is pinned to where the pack was opened. Following
        // the live eye would carry the small reconciliation corrections the
        // player gets even standing still, and right over a tarp fixed in the
        // world that reads as the whole mat jittering.
        let pinned = anchor.feet + Vec3::Y * eye_height_m();
        let kneel =
            eye.lerp(pinned, s) - Vec3::Y * (KNEEL_DROP_M * s) + forward * (KNEEL_LEAN_M * s);
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
            let lift = if self.hovered == Some(PackTarget::Item(entry.item_id)) {
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
        if self.open && self.amount >= 0.5 {
            out.extend(ring_instances(&self.ring, twine, stone));
            // Hovering "make camp here" marks out where the fire pit goes.
            if self.hovered == Some(PackTarget::MakeCamp)
                && let Some(spot) = self.camp_spot()
            {
                for i in 0..12 {
                    let angle = i as f32 / 12.0 * std::f32::consts::TAU;
                    let at = spot + Vec3::new(angle.cos(), 0.0, angle.sin()) * 0.55;
                    out.push(CubeInstance::new(
                        (at + Vec3::Y * 0.04).to_array(),
                        stone,
                        [0.1, 0.08, 0.1],
                        Quat::from_rotation_y(angle).to_array(),
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
    /// tarp when the pack is empty or more stacks exist than fit; and the
    /// camp: how far the fire is, or an offer to make camp here.
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
        let mut labels: Vec<_> = self
            .laid_out()
            .into_iter()
            .enumerate()
            .map(|(slot, entry)| {
                let height = model_height(view.item_model(entry.item_id)).max(0.1);
                let shown = entry.count.min(MAX_COPIES) as f32;
                let name = view.item_name(entry.item_id).unwrap_or("Unknown item");
                PackLabel {
                    target: Some(PackTarget::Item(entry.item_id)),
                    at: world(slot_centre(slot) + Vec3::Y * (height * shown + 0.06)),
                    text: if entry.count == 1 {
                        name.to_owned()
                    } else {
                        format!("{name} \u{d7}{}", entry.count)
                    },
                }
            })
            .collect();
        if inventory.is_empty() {
            labels.push(PackLabel {
                target: None,
                at: world(Vec3::Y * 0.05),
                text: "Nothing in the pack".to_owned(),
            });
        }
        let hidden = inventory.len().saturating_sub(COLUMNS * ROWS);
        if hidden > 0 {
            labels.push(PackLabel {
                target: None,
                at: world(Vec3::new(0.0, 0.02, TARP_D_M * 0.5 + 0.05)),
                text: format!("+{hidden} more"),
            });
        }
        if let Some((centre, radius)) = self.camp(view) {
            let distance = (centre - anchor.feet).with_y(0.0).length();
            let text = if distance <= radius {
                format!("In camp, {distance:.0} m from the fire")
            } else {
                format!("Outside your camp, {distance:.0} m from the fire")
            };
            labels.push(PackLabel {
                target: None,
                at: world(Vec3::new(-TARP_W_M * 0.5 + 0.2, 0.02, TARP_D_M * 0.5)),
                text,
            });
        } else if let Some(camp) = view.camp_tool() {
            let have = |item: u16| {
                inventory
                    .iter()
                    .find(|entry| entry.item_id == item)
                    .map_or(0, |entry| entry.count)
            };
            let affordable = camp.cost.iter().all(|&(item, count)| have(item) >= count);
            labels.push(PackLabel {
                target: affordable.then_some(PackTarget::MakeCamp),
                at: self.camp_spot().unwrap_or(origin) + Vec3::Y * 0.1,
                text: if affordable {
                    format!("Make camp here ({})", camp.cost_text)
                } else {
                    format!("Making camp needs {}", camp.cost_text)
                },
            });
        }
        labels
    }

    /// Takes the player's sites from a progression answer.
    fn take_sites(&mut self, sites: Vec<SiteEntry>) {
        if self.sites != sites {
            self.ring.clear();
        }
        self.sites = sites;
    }

    /// The player's camp: its fire's centre in world metres and its radius.
    pub(super) fn camp(&self, view: &dyn PackView) -> Option<(Vec3, f32)> {
        self.sites.iter().find_map(|site| {
            let radius = view.camp_radius_m(site.kind)?;
            let [x, y, z] = site.cell.map(|c| c as f32 * CELL_M);
            Some((Vec3::new(x + CELL_M * 0.5, y, z + CELL_M * 0.5), radius))
        })
    }

    /// Where "make camp here" builds the fire pit: on the ground just past
    /// the tarp's far edge.
    pub(super) fn camp_spot(&self) -> Option<Vec3> {
        let anchor = self.anchor?;
        Some(tarp_centre(anchor) + forward(anchor.yaw) * (TARP_D_M * 0.5 + 0.9))
    }

    /// Whether the ring around the camp still needs its ground points.
    pub(super) fn needs_ring(&self, view: &dyn PackView) -> bool {
        self.open && self.ring.is_empty() && self.camp(view).is_some()
    }

    /// The camp's boundary on the ground; `ground(x, z)` is the height of the
    /// ground there, if known.
    pub(super) fn set_ring(
        &mut self,
        view: &dyn PackView,
        ground: impl Fn(f32, f32) -> Option<f32>,
    ) {
        let Some((centre, radius)) = self.camp(view) else {
            return;
        };
        self.ring = (0..RING_PEGS)
            .map(|i| {
                let angle = i as f32 / RING_PEGS as f32 * std::f32::consts::TAU;
                let x = centre.x + radius * angle.cos();
                let z = centre.z + radius * angle.sin();
                Vec3::new(x, ground(x, z).unwrap_or(centre.y), z)
            })
            .collect();
    }

    /// [`Pack::set_ring`] on the client's replica of the terrain: each peg
    /// stands on the highest solid cell within a few metres of the fire's
    /// height.
    pub(super) fn set_ring_on(&mut self, view: &dyn PackView, terrain: &Volume) {
        let Some((centre, _)) = self.camp(view) else {
            return;
        };
        let top = ((centre.y + 8.0) / CELL_M).floor() as i64;
        let bottom = ((centre.y - 16.0) / CELL_M).floor() as i64;
        self.set_ring(view, |x, z| {
            let (cx, cz) = ((x / CELL_M).floor() as i64, (z / CELL_M).floor() as i64);
            (bottom..=top).rev().find_map(|y| {
                matches!(
                    terrain.sample(GlobalCell::new(cx, y, cz)),
                    Ok(Sample::Filled(_))
                )
                .then_some((y + 1) as f32 * CELL_M)
            })
        });
    }

    /// Asks for the inventory again on the next frame (after a build).
    pub(super) fn refresh_soon(&mut self) {
        self.requested_at = None;
    }
}

/// What a label over the pack can be clicked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PackTarget {
    Item(u16),
    MakeCamp,
}

/// Pegs with a stone at their foot around the camp, joined by twine.
fn ring_instances(ring: &[Vec3], twine: u32, stone: u32) -> Vec<CubeInstance> {
    const PEG_M: f32 = 0.45;
    let mut out = Vec::new();
    for (i, &foot) in ring.iter().enumerate() {
        out.push(CubeInstance::new(
            (foot + Vec3::Y * (PEG_M * 0.5)).to_array(),
            twine,
            [0.05, PEG_M, 0.05],
            CubeInstance::IDENTITY_ROTATION,
        ));
        out.push(CubeInstance::new(
            (foot + Vec3::Y * 0.04).to_array(),
            stone,
            [0.14, 0.08, 0.12],
            Quat::from_rotation_y(i as f32).to_array(),
        ));
        let next = ring[(i + 1) % ring.len()];
        let (a, b) = (
            foot + Vec3::Y * (PEG_M - 0.06),
            next + Vec3::Y * (PEG_M - 0.06),
        );
        let span = b - a;
        if span.length() > 0.01 {
            out.push(CubeInstance::new(
                ((a + b) * 0.5).to_array(),
                twine,
                [span.length(), 0.015, 0.015],
                Quat::from_rotation_arc(Vec3::X, span.normalize()).to_array(),
            ));
        }
    }
    out
}

/// Standing eye height above the feet, as the camera uses.
fn eye_height_m() -> f32 {
    CharacterParams::DEFAULT.total_height_m() * 0.9
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

/// The clickable label nearest the cursor within reach, if any.
pub(super) fn hovered_target(
    labels: &[(Option<PackTarget>, [f32; 2])],
    cursor: [f32; 2],
) -> Option<PackTarget> {
    labels
        .iter()
        .filter_map(|&(target, at)| {
            let target = target?;
            let distance = ((at[0] - cursor[0]).powi(2) + (at[1] - cursor[1]).powi(2)).sqrt();
            (distance <= HOVER_RADIUS_PX).then_some((target, distance))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(target, _)| target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct TestView(Vec<CubeInstance>, bool);

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
        fn camp_radius_m(&self, site_kind: u16) -> Option<f32> {
            (site_kind == 1).then_some(15.0)
        }
        fn camp_tool(&self) -> Option<CampTool> {
            self.1.then_some(CampTool {
                tool: 5,
                cost: &[(1, 4)],
                cost_text: "4 logs",
            })
        }
    }

    fn log_view() -> TestView {
        TestView(
            vec![CubeInstance::new(
                [0.0, 0.05, 0.0],
                3,
                [0.4, 0.1, 0.1],
                CubeInstance::IDENTITY_ROTATION,
            )],
            false,
        )
    }

    fn camp_view() -> TestView {
        TestView(log_view().0, true)
    }

    fn fire_pit(cell: [i64; 3]) -> SiteEntry {
        SiteEntry {
            site_id: 1,
            kind: 1,
            cell,
            facing: 0,
        }
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
        // Off by a correction's few centimetres from the opening pose.
        let eye = Vec3::new(10.03, 5.0 + eye_height_m() + 0.02, -3.0);
        let ahead = Vec3::new(0.0, 0.0, -1.0);
        assert_eq!(pack.camera(eye, ahead), (eye, ahead));

        pack.open([10.0, 5.0, -3.0], 0.0);
        pack.advance(UNROLL_S * 2.0);
        let (kneel, look) = pack.camera(eye, ahead);
        let pinned = Vec3::new(
            10.0,
            5.0 + eye_height_m() - KNEEL_DROP_M,
            -3.0 - KNEEL_LEAN_M,
        );
        assert!(kneel.distance(pinned) < 1e-4, "{kneel:?}");
        // Corrections to the live eye no longer move the kneeling camera.
        assert_eq!(pack.camera(eye + Vec3::X * 0.05, ahead).0, kneel);
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
        assert_eq!(labels[0].target, None);

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
            sites: Vec::new(),
        };
        pack.sync_inventory(Some(reply), 1, now + REFRESH);
        assert_eq!(pack.laid_out().len(), 1);
    }

    #[test]
    fn hover_picks_the_nearest_item_within_reach() {
        let labels = [
            (Some(PackTarget::Item(1)), [100.0, 100.0]),
            (Some(PackTarget::MakeCamp), [160.0, 100.0]),
            (None, [130.0, 100.0]),
        ];
        assert_eq!(
            hovered_target(&labels, [120.0, 100.0]),
            Some(PackTarget::Item(1))
        );
        assert_eq!(
            hovered_target(&labels, [150.0, 110.0]),
            Some(PackTarget::MakeCamp)
        );
        assert_eq!(hovered_target(&labels, [400.0, 400.0]), None);
    }

    #[test]
    fn without_a_camp_the_tarp_offers_one_only_when_it_is_affordable() {
        let view = camp_view();
        let poor = opened(vec![InventoryEntry {
            item_id: 1,
            count: 3,
        }]);
        let offer = poor.labels(&view).pop().unwrap();
        assert_eq!(
            (offer.target, offer.text.as_str()),
            (None, "Making camp needs 4 logs")
        );
        let rich = opened(vec![InventoryEntry {
            item_id: 1,
            count: 4,
        }]);
        let offer = rich.labels(&view).pop().unwrap();
        assert_eq!(offer.target, Some(PackTarget::MakeCamp));
        // The offer stands where the fire pit would go, past the tarp.
        let ahead = (offer.at - Vec3::new(10.0, 5.0, -3.0)).dot(forward(0.0));
        assert!(ahead > TARP_NEAR_M + TARP_D_M, "{ahead} m ahead");
        // No offer from a game without a camp tool.
        assert!(
            rich.labels(&log_view())
                .iter()
                .all(|l| l.target != Some(PackTarget::MakeCamp))
        );
    }

    #[test]
    fn with_a_camp_the_tarp_says_how_far_the_fire_is_and_rings_the_camp() {
        let view = camp_view();
        let mut pack = opened(Vec::new());
        // The pack opened at (10, 5, -3); the fire is 6 m east of it.
        let fire = [
            (16.0 / CELL_M) as i64,
            (5.0 / CELL_M) as i64,
            (-3.0 / CELL_M) as i64,
        ];
        pack.take_sites(vec![fire_pit(fire)]);
        let texts: Vec<_> = pack.labels(&view).into_iter().map(|l| l.text).collect();
        assert!(
            texts.contains(&"In camp, 6 m from the fire".to_owned()),
            "{texts:?}"
        );
        assert!(!texts.iter().any(|t| t.contains("Make camp")));

        assert!(pack.needs_ring(&view));
        pack.set_ring(&view, |_, _| Some(4.5));
        assert!(!pack.needs_ring(&view));
        let (centre, radius) = pack.camp(&view).unwrap();
        let pegs: Vec<_> = pack
            .instances(&view)
            .into_iter()
            .filter(|c| c.material == 8 && c.size[1] > 0.4)
            .collect();
        assert_eq!(pegs.len(), RING_PEGS);
        for peg in pegs {
            let foot = Vec3::from_array(peg.offset) - Vec3::Y * (peg.size[1] * 0.5);
            assert!(((foot - centre).with_y(0.0).length() - radius).abs() < 0.01);
            assert!(
                (foot.y - 4.5).abs() < 1e-4,
                "stands on the ground it was given"
            );
        }

        // Moving the camp asks for a new ring.
        pack.take_sites(vec![fire_pit([400, 20, 0])]);
        assert!(pack.needs_ring(&view));
        let far: Vec<_> = pack.labels(&view).into_iter().map(|l| l.text).collect();
        assert!(
            far.iter().any(|t| t.starts_with("Outside your camp")),
            "{far:?}"
        );
    }
}
