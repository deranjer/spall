//! The pack (Tab): a canvas tarp slides up over the screen with the
//! inventory laid out on it as the game's item icons. It is UI only; the
//! world keeps running behind it. The inventory comes from the
//! [`InventoryFeed`] while the pack is open.

use super::*;
use spall_protocol::InventoryEntry;

/// Seconds for the tarp to slide in, or back out.
const SLIDE_S: f32 = 0.3;
/// Item stacks laid out per row, and rows that fit on the tarp.
const COLUMNS: usize = 4;
const ROWS: usize = 3;
/// The tarp art's width over its height (`assets/hud/tarp.png`).
const TARP_ASPECT: f32 = 1200.0 / 820.0;
/// The tarp's share of the screen: at most this much of its width or height.
const TARP_MAX_WIDTH: f32 = 0.66;
const TARP_MAX_HEIGHT: f32 = 0.8;
/// Inside the tarp's hem: the items' area as a share of the tarp.
const TARP_INSET_X: f32 = 0.09;
const TARP_INSET_Y: f32 = 0.11;
/// An item's icon as a share of its slot's smaller side.
const ICON_SHARE: f32 = 0.6;

/// A rectangle in logical pixels from the top left of the screen.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct UiRect {
    pub(super) x: f32,
    pub(super) y: f32,
    pub(super) w: f32,
    pub(super) h: f32,
}

impl UiRect {
    pub(super) fn contains(self, [x, y]: [f32; 2]) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}

/// One frame of the pack, laid out for the screen.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct PackFrame {
    /// 0 hidden below the screen, 1 fully in (eased).
    pub(super) shown: f32,
    pub(super) tarp: UiRect,
    pub(super) slots: Vec<PackSlot>,
    /// A line centred on the tarp: an empty pack, or stacks that do not fit.
    pub(super) note: Option<String>,
}

/// One item stack on the tarp.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct PackSlot {
    pub(super) item_id: u16,
    /// The whole slot (hover area); the icon sits centred near its top.
    pub(super) cell: UiRect,
    pub(super) icon: UiRect,
    pub(super) icon_png: Option<&'static [u8]>,
    pub(super) name: String,
    pub(super) count: u32,
    pub(super) hovered: bool,
}

pub(super) struct Pack {
    open: bool,
    /// 0 out of sight, 1 fully in (linear in time).
    amount: f32,
    hovered: Option<u16>,
}

impl Pack {
    pub(super) fn new() -> Self {
        Self {
            open: false,
            amount: 0.0,
            hovered: None,
        }
    }

    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    /// Open, or still sliding away.
    pub(super) fn is_shown(&self) -> bool {
        self.open || self.amount > 0.0
    }

    pub(super) fn open(&mut self) {
        self.open = true;
    }

    pub(super) fn close(&mut self) {
        self.open = false;
        self.hovered = None;
    }

    /// Steps the slide by `dt_s`.
    pub(super) fn advance(&mut self, dt_s: f32) {
        let step = dt_s.max(0.0) / SLIDE_S;
        self.amount = if self.open {
            (self.amount + step).min(1.0)
        } else {
            (self.amount - step).max(0.0)
        };
    }

    fn eased(&self) -> f32 {
        let t = self.amount;
        t * t * (3.0 - 2.0 * t)
    }

    /// The stacks that fit on the tarp, in slot order.
    fn laid_out(stacks: Option<&[InventoryEntry]>) -> &[InventoryEntry] {
        let all = stacks.unwrap_or_default();
        &all[..all.len().min(COLUMNS * ROWS)]
    }

    /// The item under the cursor as of the last [`Self::hover`].
    pub(super) fn hovered_item(&self) -> Option<u16> {
        self.hovered
    }

    /// Hovers the stack under `cursor` (logical pixels), if any.
    pub(super) fn hover(
        &mut self,
        screen: [f32; 2],
        cursor: [f32; 2],
        stacks: Option<&[InventoryEntry]>,
    ) {
        self.hovered = None;
        if !self.open {
            return;
        }
        let tarp = tarp_rect(screen, self.eased());
        self.hovered = Self::laid_out(stacks)
            .iter()
            .enumerate()
            .find(|(slot, _)| slot_cell(tarp, *slot).contains(cursor))
            .map(|(_, entry)| entry.item_id);
    }

    /// This frame's pack on a `screen` of logical pixels, or `None` when
    /// it is out of sight.
    pub(super) fn frame(
        &self,
        view: &dyn PackView,
        screen: [f32; 2],
        stacks: Option<&[InventoryEntry]>,
    ) -> Option<PackFrame> {
        if !self.is_shown() {
            return None;
        }
        let shown = self.eased();
        let tarp = tarp_rect(screen, shown);
        let slots = Self::laid_out(stacks)
            .iter()
            .enumerate()
            .map(|(slot, entry)| {
                let cell = slot_cell(tarp, slot);
                PackSlot {
                    item_id: entry.item_id,
                    cell,
                    icon: icon_rect(cell),
                    icon_png: view.item_icon_png(entry.item_id),
                    name: view
                        .item_name(entry.item_id)
                        .unwrap_or("Unknown item")
                        .to_owned(),
                    count: entry.count,
                    hovered: self.hovered == Some(entry.item_id),
                }
            })
            .collect();
        let note = match stacks {
            Some([]) => Some("Nothing in the pack".to_owned()),
            Some(stacks) if stacks.len() > COLUMNS * ROWS => {
                Some(format!("+{} more", stacks.len() - COLUMNS * ROWS))
            }
            _ => None,
        };
        Some(PackFrame {
            shown,
            tarp,
            slots,
            note,
        })
    }
}

/// The tarp, centred, rising from below the screen as `shown` goes 0 to 1.
fn tarp_rect([sw, sh]: [f32; 2], shown: f32) -> UiRect {
    let w = (sw * TARP_MAX_WIDTH).min(sh * TARP_MAX_HEIGHT * TARP_ASPECT);
    let h = w / TARP_ASPECT;
    let rest = (sh - h) * 0.5;
    UiRect {
        x: (sw - w) * 0.5,
        y: rest + (sh - rest) * (1.0 - shown),
        w,
        h,
    }
}

/// Slot `slot` inside the tarp's hem: rows left to right, top to bottom.
fn slot_cell(tarp: UiRect, slot: usize) -> UiRect {
    let x0 = tarp.x + tarp.w * TARP_INSET_X;
    let y0 = tarp.y + tarp.h * TARP_INSET_Y;
    let w = tarp.w * (1.0 - 2.0 * TARP_INSET_X) / COLUMNS as f32;
    let h = tarp.h * (1.0 - 2.0 * TARP_INSET_Y) / ROWS as f32;
    UiRect {
        x: x0 + w * (slot % COLUMNS) as f32,
        y: y0 + h * (slot / COLUMNS) as f32,
        w,
        h,
    }
}

/// The icon square, centred across the slot and near its top, leaving room
/// for the name below.
fn icon_rect(cell: UiRect) -> UiRect {
    let side = cell.w.min(cell.h) * ICON_SHARE;
    UiRect {
        x: cell.x + (cell.w - side) * 0.5,
        y: cell.y + cell.h * 0.06,
        w: side,
        h: side,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct TestView;

    static LOG_PNG: [u8; 4] = [1, 2, 3, 4];

    impl PackView for TestView {
        fn catalog_version(&self) -> u32 {
            1
        }
        fn item_name(&self, item_id: u16) -> Option<&str> {
            (item_id == 1).then_some("Log")
        }
        fn item_icon_png(&self, item_id: u16) -> Option<&'static [u8]> {
            (item_id == 1).then_some(&LOG_PNG[..])
        }
        fn blueprints(&self) -> &[Blueprint] {
            &[]
        }
    }

    const SCREEN: [f32; 2] = [1280.0, 720.0];

    fn opened() -> Pack {
        let mut pack = Pack::new();
        pack.open();
        pack.advance(SLIDE_S);
        pack
    }

    fn stacks(ids: std::ops::RangeInclusive<u16>) -> Vec<InventoryEntry> {
        ids.map(|item_id| InventoryEntry { item_id, count: 1 })
            .collect()
    }

    #[test]
    fn the_tarp_slides_up_into_the_middle_and_back_out() {
        let mut pack = Pack::new();
        assert!(
            pack.frame(&TestView, SCREEN, None).is_none(),
            "closed: nothing drawn"
        );
        pack.open();
        pack.advance(SLIDE_S * 0.5);
        let rising = pack.frame(&TestView, SCREEN, None).unwrap().tarp;
        pack.advance(SLIDE_S);
        let tarp = pack.frame(&TestView, SCREEN, None).unwrap().tarp;
        assert!(rising.y > tarp.y, "still rising half way");
        assert!((tarp.x + tarp.w * 0.5 - SCREEN[0] * 0.5).abs() < 0.01);
        assert!((tarp.y + tarp.h * 0.5 - SCREEN[1] * 0.5).abs() < 0.01);
        assert!(tarp.w <= SCREEN[0] * TARP_MAX_WIDTH + 0.01);
        assert!(tarp.h <= SCREEN[1] * TARP_MAX_HEIGHT + 0.01);
        assert!(
            (tarp.w / tarp.h - TARP_ASPECT).abs() < 1e-3,
            "keeps the art's shape"
        );

        pack.close();
        assert!(pack.is_shown(), "slides away over time, not at once");
        pack.advance(SLIDE_S);
        assert!(pack.frame(&TestView, SCREEN, None).is_none());
    }

    #[test]
    fn stacks_fill_the_tarp_in_rows_inside_its_hem_with_names_and_counts() {
        let pack = opened();
        let two = [
            InventoryEntry {
                item_id: 1,
                count: 12,
            },
            InventoryEntry {
                item_id: 4,
                count: 1,
            },
        ];
        let frame = pack.frame(&TestView, SCREEN, Some(&two)).unwrap();
        assert_eq!(frame.note, None);
        let [log, other] = &frame.slots[..] else {
            panic!("two slots: {:?}", frame.slots)
        };
        assert_eq!((log.name.as_str(), log.count), ("Log", 12));
        assert_eq!(log.icon_png, Some(&LOG_PNG[..]));
        assert_eq!(
            (other.name.as_str(), other.icon_png),
            ("Unknown item", None)
        );
        assert!(
            other.cell.x > log.cell.x && other.cell.y == log.cell.y,
            "same row"
        );

        let full = stacks(1..=12);
        let frame = pack.frame(&TestView, SCREEN, Some(&full)).unwrap();
        let tarp = frame.tarp;
        for slot in &frame.slots {
            let UiRect { x, y, w, h } = slot.cell;
            assert!(x >= tarp.x + tarp.w * 0.05 && x + w <= tarp.x + tarp.w * 0.95);
            assert!(y >= tarp.y + tarp.h * 0.05 && y + h <= tarp.y + tarp.h * 0.95);
            let icon = slot.icon;
            assert!(icon.x >= x && icon.x + icon.w <= x + w && icon.y + icon.h <= y + h);
        }
        assert!(
            frame.slots[4].cell.y > frame.slots[0].cell.y,
            "the fifth starts row two"
        );
    }

    #[test]
    fn notes_cover_an_empty_pack_and_stacks_that_do_not_fit() {
        let pack = opened();
        let frame = pack.frame(&TestView, SCREEN, Some(&[])).unwrap();
        assert!(frame.slots.is_empty());
        assert_eq!(frame.note.as_deref(), Some("Nothing in the pack"));

        let many = stacks(1..=15);
        let frame = pack.frame(&TestView, SCREEN, Some(&many)).unwrap();
        assert_eq!(frame.slots.len(), COLUMNS * ROWS);
        assert_eq!(frame.note.as_deref(), Some("+3 more"));

        let frame = pack.frame(&TestView, SCREEN, None).unwrap();
        assert!(
            frame.slots.is_empty() && frame.note.is_none(),
            "no answer yet"
        );
    }

    #[test]
    fn the_cursor_hovers_the_stack_under_it_only_while_open() {
        let mut pack = opened();
        let three = stacks(1..=3);
        let three = Some(&three[..]);
        let second = pack.frame(&TestView, SCREEN, three).unwrap().slots[1].cell;
        let inside = [second.x + second.w * 0.5, second.y + second.h * 0.5];
        pack.hover(SCREEN, inside, three);
        let frame = pack.frame(&TestView, SCREEN, three).unwrap();
        assert_eq!(
            frame.slots.iter().map(|s| s.hovered).collect::<Vec<_>>(),
            [false, true, false]
        );
        pack.hover(SCREEN, [1.0, 1.0], three);
        let hovered = |pack: &Pack| {
            pack.frame(&TestView, SCREEN, three)
                .unwrap()
                .slots
                .iter()
                .any(|s| s.hovered)
        };
        assert!(!hovered(&pack));
        pack.hover(SCREEN, inside, three);
        pack.close();
        pack.hover(SCREEN, inside, three);
        assert!(!hovered(&pack));
    }
}
