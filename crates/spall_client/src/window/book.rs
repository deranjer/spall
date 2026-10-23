//! The blueprint book (B): an open journal of the game's blueprints, each
//! showing where its raw materials go and how much of each it needs against
//! how much the player has. Clicking one turns to the crafting cloth: the
//! player drags raw materials from the pack onto the cloth, where each snaps
//! into a slot that takes it, seen from above or from the side as the
//! blueprint says. Once every slot is filled, Craft asks the server for it.
//! UI only; the world keeps running.

use super::*;

/// Seconds for the book to slide in, or back out.
const SLIDE_S: f32 = 0.3;
/// The book art's width over its height (`assets/hud/book.png`).
const BOOK_ASPECT: f32 = 1400.0 / 900.0;
const BOOK_MAX_WIDTH: f32 = 0.84;
const BOOK_MAX_HEIGHT: f32 = 0.88;
/// The pages in the art, as fractions of the book: left x, right x, y, and
/// each page's width and height.
const LEFT_PAGE_X: f32 = 52.0 / 1400.0;
const RIGHT_PAGE_X: f32 = 700.0 / 1400.0;
const PAGE_Y: f32 = 56.0 / 900.0;
const PAGE_W: f32 = 636.0 / 1400.0;
const PAGE_H: f32 = 780.0 / 900.0;
/// Blueprint cards per page: columns and rows.
const CARD_COLUMNS: usize = 2;
const CARD_ROWS: usize = 3;
const CARDS: usize = 2 * CARD_COLUMNS * CARD_ROWS;
/// Material slots along the strip under the cloth.
const MATERIAL_SLOTS: usize = 6;
/// A dropped material snaps into a slot whose centre is within the slot's
/// half size plus this share of the cloth.
const SNAP_REACH: f32 = 0.08;
/// A press and release this close together (logical pixels) is a click.
const CLICK_SLOP_PX: f32 = 5.0;

/// One frame of the book, laid out for the screen.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct BookFrame {
    /// 0 hidden below the screen, 1 fully in (eased).
    pub(super) shown: f32,
    pub(super) book: UiRect,
    pub(super) page: BookPage,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum BookPage {
    /// Every blueprint as a card across both pages.
    Contents {
        cards: Vec<BlueprintCard>,
        note: Option<String>,
    },
    /// One blueprint on the left page, the cloth on the right.
    Cloth(Box<ClothPage>),
}

/// An item to draw: its icon and a count (0 draws no count).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ItemDraw {
    pub(super) item_id: u16,
    pub(super) icon_png: Option<&'static [u8]>,
    pub(super) count: u32,
}

/// A raw material a blueprint needs: what, how many, and how many the player
/// has.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct NeedLine {
    pub(super) name: String,
    pub(super) need: u32,
    pub(super) have: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct BlueprintCard {
    pub(super) rect: UiRect,
    pub(super) hovered: bool,
    pub(super) name: String,
    pub(super) output: ItemDraw,
    pub(super) view: BlueprintView,
    /// The little drawing of the layout, and each slot in it.
    pub(super) diagram: UiRect,
    pub(super) pattern: Vec<(UiRect, ItemDraw)>,
    pub(super) needs: Vec<NeedLine>,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct ClothPage {
    pub(super) back: UiRect,
    pub(super) back_hovered: bool,
    /// The chosen blueprint, drawn large on the left page.
    pub(super) blueprint: BlueprintCard,
    pub(super) cloth: UiRect,
    pub(super) view: BlueprintView,
    pub(super) slots: Vec<ClothSlot>,
    pub(super) materials: Vec<MaterialSlot>,
    pub(super) craft: UiRect,
    /// Every slot is filled.
    pub(super) ready: bool,
    pub(super) craft_hovered: bool,
    /// A craft is waiting on the server.
    pub(super) crafting: bool,
    pub(super) note: Option<String>,
    /// The material being dragged, under the cursor.
    pub(super) dragging: Option<(UiRect, ItemDraw)>,
}

/// One slot on the cloth: the item it takes, and whether one lies there.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ClothSlot {
    pub(super) rect: UiRect,
    pub(super) item: ItemDraw,
    pub(super) filled: bool,
    pub(super) hovered: bool,
    /// Where the material being dragged would snap if dropped now.
    pub(super) snap: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct MaterialSlot {
    pub(super) rect: UiRect,
    pub(super) item: ItemDraw,
    pub(super) name: String,
    pub(super) hovered: bool,
}

/// What a click or drop in the book asks of the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BookAction {
    /// One batch of this recipe.
    Craft(u16),
}

/// What is under the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Blueprint(usize),
    Back,
    Material(u16),
    Slot(usize),
    Craft,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Drag {
    item: u16,
    /// The slot it was picked up from, or `None` from the strip.
    from: Option<usize>,
    /// Where the press happened, to tell a click from a drag.
    pressed_at: [f32; 2],
}

pub(super) struct Book {
    open: bool,
    /// 0 out of sight, 1 fully in (linear in time).
    amount: f32,
    /// The blueprint whose cloth is open.
    selected: Option<usize>,
    /// Which of the selected blueprint's slots hold their item.
    filled: Vec<bool>,
    drag: Option<Drag>,
    pressed: Option<Target>,
    screen: [f32; 2],
    cursor: [f32; 2],
    note: Option<String>,
}

impl Book {
    pub(super) fn new() -> Self {
        Self {
            open: false,
            amount: 0.0,
            selected: None,
            filled: Vec::new(),
            drag: None,
            pressed: None,
            screen: [0.0; 2],
            cursor: [-1.0; 2],
            note: None,
        }
    }

    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    /// Open, or still sliding away.
    pub(super) fn is_shown(&self) -> bool {
        self.open || self.amount > 0.0
    }

    /// Opens at the contents; materials left on the cloth go back.
    pub(super) fn open(&mut self) {
        self.open = true;
        self.select(None);
        self.pressed = None;
    }

    pub(super) fn close(&mut self) {
        self.open = false;
        self.drag = None;
        self.pressed = None;
    }

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

    fn select(&mut self, index: Option<usize>) {
        self.selected = index;
        self.filled.clear();
        self.drag = None;
        self.note = None;
    }

    /// The cursor moved (logical pixels on a `screen` of logical pixels).
    pub(super) fn pointer(&mut self, screen: [f32; 2], cursor: [f32; 2]) {
        self.screen = screen;
        self.cursor = cursor;
    }

    fn blueprint<'v>(&self, view: &'v dyn PackView) -> Option<&'v Blueprint> {
        view.blueprints().get(self.selected?)
    }

    fn is_filled(&self, slot: usize) -> bool {
        self.filled.get(slot).copied().unwrap_or(false)
    }

    fn set_filled(&mut self, slot: usize, filled: bool) {
        if self.filled.len() <= slot {
            self.filled.resize(slot + 1, false);
        }
        self.filled[slot] = filled;
    }

    /// How many of `item_id` lie on the cloth (or are being dragged).
    fn placed(&self, blueprint: Option<&Blueprint>, item_id: u16) -> u32 {
        let on_cloth = blueprint.map_or(0, |blueprint| {
            blueprint
                .slots
                .iter()
                .enumerate()
                .filter(|(i, slot)| slot.item == item_id && self.is_filled(*i))
                .count()
        });
        on_cloth as u32 + u32::from(self.drag.is_some_and(|drag| drag.item == item_id))
    }

    /// How many of `item_id` are still in the pack, off the cloth.
    fn available(&self, blueprint: Option<&Blueprint>, item_id: u16, feed: &InventoryFeed) -> u32 {
        feed.count(item_id)
            .saturating_sub(self.placed(blueprint, item_id))
    }

    /// Takes back anything on the cloth the inventory no longer has (it was
    /// crafted, or used elsewhere).
    pub(super) fn reconcile(&mut self, view: &dyn PackView, feed: &InventoryFeed) {
        if feed.stacks().is_none() {
            return;
        }
        let Some(blueprint) = self.blueprint(view) else {
            return;
        };
        for i in 0..blueprint.slots.len() {
            let item = blueprint.slots[i].item;
            if self.is_filled(i) && self.placed(Some(blueprint), item) > feed.count(item) {
                self.set_filled(i, false);
            }
        }
    }

    pub(super) fn press(&mut self, view: &dyn PackView, feed: &InventoryFeed) {
        if !self.open {
            return;
        }
        let target = self.target(view, feed);
        self.pressed = target;
        let blueprint = self.blueprint(view);
        match target {
            Some(Target::Blueprint(index)) => self.select(Some(index)),
            Some(Target::Back) => self.select(None),
            Some(Target::Material(item)) if self.available(blueprint, item, feed) > 0 => {
                self.drag = Some(Drag {
                    item,
                    from: None,
                    pressed_at: self.cursor,
                });
                self.note = None;
            }
            Some(Target::Slot(index)) if self.is_filled(index) => {
                let item = blueprint.expect("a slot means a blueprint").slots[index].item;
                self.set_filled(index, false);
                self.drag = Some(Drag {
                    item,
                    from: Some(index),
                    pressed_at: self.cursor,
                });
                self.note = None;
            }
            _ => {}
        }
    }

    /// The button came up: drops a dragged material, or presses Craft.
    pub(super) fn release(
        &mut self,
        view: &dyn PackView,
        feed: &InventoryFeed,
    ) -> Option<BookAction> {
        let target = self.target(view, feed);
        let pressed = self.pressed.take();
        if let Some(drag) = self.drag.take() {
            let moved =
                (drag.pressed_at[0] - self.cursor[0]).hypot(drag.pressed_at[1] - self.cursor[1]);
            // Clicking a laid material (no drag) takes it back to the pack.
            let clicked_off = drag.from.is_some() && moved < CLICK_SLOP_PX;
            if !clicked_off && let Some(slot) = self.snap_slot(view, drag.item) {
                self.set_filled(slot, true);
            }
            return None;
        }
        if pressed == Some(Target::Craft)
            && target == Some(Target::Craft)
            && !feed.crafting()
            && let Some(blueprint) = self.blueprint(view)
            && self.complete(blueprint)
        {
            self.note = Some("Crafting\u{2026}".to_owned());
            return Some(BookAction::Craft(blueprint.recipe_id));
        }
        None
    }

    fn complete(&self, blueprint: &Blueprint) -> bool {
        (0..blueprint.slots.len()).all(|i| self.is_filled(i))
    }

    /// The empty slot taking `item` that a drop at the cursor snaps into:
    /// the nearest within reach.
    fn snap_slot(&self, view: &dyn PackView, item: u16) -> Option<usize> {
        let blueprint = self.blueprint(view)?;
        let cloth = cloth_rect(pages(book_rect(self.screen, self.eased())).1);
        let [x, y] = self.cursor;
        blueprint
            .slots
            .iter()
            .enumerate()
            .filter(|(i, slot)| slot.item == item && !self.is_filled(*i))
            .filter_map(|(i, slot)| {
                let centre = [
                    cloth.x + slot.at[0] * cloth.w,
                    cloth.y + slot.at[1] * cloth.h,
                ];
                let distance = (centre[0] - x).hypot(centre[1] - y);
                let reach = (slot.size * 0.5 + SNAP_REACH) * cloth.w;
                (distance <= reach).then_some((i, distance))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(i, _)| i)
    }

    /// The server answered the craft.
    pub(super) fn crafted(&mut self, outcome: &CraftOutcome, view: &dyn PackView) {
        self.note = Some(match outcome {
            CraftOutcome::Crafted { recipe_id } => {
                self.filled.clear();
                match view
                    .blueprints()
                    .iter()
                    .find(|blueprint| blueprint.recipe_id == *recipe_id)
                {
                    Some(blueprint) => {
                        let (item, count) = blueprint.output;
                        format!("Made {count} {}", view.item_name(item).unwrap_or("items"))
                    }
                    None => "Made it".to_owned(),
                }
            }
            CraftOutcome::Rejected(spall_protocol::ProgressionRejectCode::InsufficientItems) => {
                "Not enough materials".to_owned()
            }
            CraftOutcome::Rejected(spall_protocol::ProgressionRejectCode::CatalogVersion) => {
                "This blueprint is out of date".to_owned()
            }
            CraftOutcome::Rejected(_) => "Couldn't craft that, try again".to_owned(),
        });
    }

    /// What lies under the cursor in the current layout.
    fn target(&self, view: &dyn PackView, feed: &InventoryFeed) -> Option<Target> {
        let book = book_rect(self.screen, self.eased());
        let at = self.cursor;
        let Some(blueprint) = self.blueprint(view) else {
            return (0..view.blueprints().len().min(CARDS))
                .find(|&i| card_rect(book, i).contains(at))
                .map(Target::Blueprint);
        };
        let (left, right) = pages(book);
        if back_rect(left).contains(at) {
            return Some(Target::Back);
        }
        let cloth = cloth_rect(right);
        // The topmost (last drawn) filled slot under the cursor, else any.
        let under: Vec<usize> = (0..blueprint.slots.len())
            .filter(|&i| slot_rect(cloth, &blueprint.slots[i]).contains(at))
            .collect();
        if let Some(&i) = under
            .iter()
            .rev()
            .find(|&&i| self.is_filled(i))
            .or(under.last())
        {
            return Some(Target::Slot(i));
        }
        if craft_rect(right).contains(at) {
            return Some(Target::Craft);
        }
        materials(feed)
            .enumerate()
            .find(|(i, _)| material_rect(right, *i).contains(at))
            .map(|(_, item)| Target::Material(item))
    }

    /// This frame's book, or `None` when it is out of sight.
    pub(super) fn frame(&self, view: &dyn PackView, feed: &InventoryFeed) -> Option<BookFrame> {
        if !self.is_shown() {
            return None;
        }
        let shown = self.eased();
        let book = book_rect(self.screen, shown);
        let hovered = self.target(view, feed).filter(|_| self.open);
        let draw = |item_id: u16, count: u32| ItemDraw {
            item_id,
            icon_png: view.item_icon_png(item_id),
            count,
        };
        let page = match self.blueprint(view) {
            None => {
                let all = view.blueprints();
                BookPage::Contents {
                    cards: all
                        .iter()
                        .take(CARDS)
                        .enumerate()
                        .map(|(i, blueprint)| {
                            card(
                                view,
                                feed,
                                blueprint,
                                card_rect(book, i),
                                hovered == Some(Target::Blueprint(i)),
                            )
                        })
                        .collect(),
                    note: if all.is_empty() {
                        Some("No blueprints yet".to_owned())
                    } else if all.len() > CARDS {
                        Some(format!("+{} more", all.len() - CARDS))
                    } else {
                        None
                    },
                }
            }
            Some(blueprint) => {
                let (left, right) = pages(book);
                let back = back_rect(left);
                let below_back = UiRect {
                    y: back.y + back.h * 1.4,
                    h: left.h - (back.y + back.h * 1.4 - left.y) - left.h * 0.3,
                    ..left
                };
                let cloth = cloth_rect(right);
                let snap = self.drag.and_then(|drag| self.snap_slot(view, drag.item));
                BookPage::Cloth(Box::new(ClothPage {
                    back,
                    back_hovered: hovered == Some(Target::Back),
                    blueprint: card(view, feed, blueprint, inset(below_back, 0.06), false),
                    cloth,
                    view: blueprint.view,
                    slots: blueprint
                        .slots
                        .iter()
                        .enumerate()
                        .map(|(i, slot)| ClothSlot {
                            rect: slot_rect(cloth, slot),
                            item: draw(slot.item, 0),
                            filled: self.is_filled(i),
                            hovered: hovered == Some(Target::Slot(i)),
                            snap: snap == Some(i),
                        })
                        .collect(),
                    materials: materials(feed)
                        .enumerate()
                        .map(|(i, item)| MaterialSlot {
                            rect: material_rect(right, i),
                            item: draw(item, self.available(Some(blueprint), item, feed)),
                            name: view.item_name(item).unwrap_or("Unknown item").to_owned(),
                            hovered: hovered == Some(Target::Material(item)),
                        })
                        .collect(),
                    craft: craft_rect(right),
                    ready: self.complete(blueprint),
                    craft_hovered: hovered == Some(Target::Craft),
                    crafting: feed.crafting(),
                    note: self.note.clone(),
                    dragging: self.drag.map(|drag| {
                        let size = blueprint
                            .slots
                            .iter()
                            .find(|slot| slot.item == drag.item)
                            .map_or(0.16, |slot| slot.size);
                        let side = size * cloth.w;
                        (
                            UiRect {
                                x: self.cursor[0] - side * 0.5,
                                y: self.cursor[1] - side * 0.5,
                                w: side,
                                h: side,
                            },
                            draw(drag.item, 0),
                        )
                    }),
                }))
            }
        };
        Some(BookFrame { shown, book, page })
    }
}

impl BookFrame {
    /// Every item icon this frame draws.
    pub(super) fn icons(&self) -> Vec<(u16, Option<&'static [u8]>)> {
        let card_icons = |card: &BlueprintCard| {
            std::iter::once(&card.output)
                .chain(card.pattern.iter().map(|(_, item)| item))
                .map(|item| (item.item_id, item.icon_png))
                .collect::<Vec<_>>()
        };
        match &self.page {
            BookPage::Contents { cards, .. } => cards.iter().flat_map(card_icons).collect(),
            BookPage::Cloth(page) => card_icons(&page.blueprint)
                .into_iter()
                .chain(
                    page.materials
                        .iter()
                        .map(|slot| (slot.item.item_id, slot.item.icon_png)),
                )
                .collect(),
        }
    }
}

/// A blueprint drawn in `rect`: name, layout, needs against the inventory.
fn card(
    view: &dyn PackView,
    feed: &InventoryFeed,
    blueprint: &Blueprint,
    rect: UiRect,
    hovered: bool,
) -> BlueprintCard {
    let (item, count) = blueprint.output;
    // The layout drawing: a square in the card's left half, below the name.
    let side = (rect.w * 0.44).min(rect.h * 0.66);
    let diagram = UiRect {
        x: rect.x + rect.w * 0.05,
        y: rect.y + rect.h * 0.27,
        w: side,
        h: side,
    };
    BlueprintCard {
        rect,
        hovered,
        name: view.item_name(item).unwrap_or("Unknown item").to_owned(),
        output: ItemDraw {
            item_id: item,
            icon_png: view.item_icon_png(item),
            count,
        },
        view: blueprint.view,
        diagram,
        pattern: blueprint
            .slots
            .iter()
            .map(|slot| {
                (
                    slot_rect(diagram, slot),
                    ItemDraw {
                        item_id: slot.item,
                        icon_png: view.item_icon_png(slot.item),
                        count: 0,
                    },
                )
            })
            .collect(),
        needs: blueprint
            .needs()
            .into_iter()
            .map(|(item, need)| NeedLine {
                name: view.item_name(item).unwrap_or("Unknown item").to_owned(),
                need,
                have: feed.count(item),
            })
            .collect(),
    }
}

/// The raw materials the strip offers: everything in the pack, in order.
fn materials(feed: &InventoryFeed) -> impl Iterator<Item = u16> + '_ {
    feed.stacks()
        .unwrap_or_default()
        .iter()
        .take(MATERIAL_SLOTS)
        .map(|entry| entry.item_id)
}

fn book_rect([sw, sh]: [f32; 2], shown: f32) -> UiRect {
    let w = (sw * BOOK_MAX_WIDTH).min(sh * BOOK_MAX_HEIGHT * BOOK_ASPECT);
    let h = w / BOOK_ASPECT;
    let rest = (sh - h) * 0.5;
    UiRect {
        x: (sw - w) * 0.5,
        y: rest + (sh - rest) * (1.0 - shown),
        w,
        h,
    }
}

fn pages(book: UiRect) -> (UiRect, UiRect) {
    let page = |x: f32| UiRect {
        x: book.x + book.w * x,
        y: book.y + book.h * PAGE_Y,
        w: book.w * PAGE_W,
        h: book.h * PAGE_H,
    };
    (page(LEFT_PAGE_X), page(RIGHT_PAGE_X))
}

fn inset(rect: UiRect, share: f32) -> UiRect {
    let (dx, dy) = (rect.w * share, rect.h * share);
    UiRect {
        x: rect.x + dx,
        y: rect.y + dy,
        w: rect.w - 2.0 * dx,
        h: rect.h - 2.0 * dy,
    }
}

/// Contents card `index`: the left page's grid first, then the right's.
fn card_rect(book: UiRect, index: usize) -> UiRect {
    let (left, right) = pages(book);
    let page = inset(if index < CARDS / 2 { left } else { right }, 0.05);
    let local = index % (CARDS / 2);
    let w = page.w / CARD_COLUMNS as f32;
    let h = page.h / CARD_ROWS as f32;
    inset(
        UiRect {
            x: page.x + w * (local % CARD_COLUMNS) as f32,
            y: page.y + h * (local / CARD_COLUMNS) as f32,
            w,
            h,
        },
        0.05,
    )
}

/// "Back to blueprints", top left of the left page.
fn back_rect(left: UiRect) -> UiRect {
    UiRect {
        x: left.x + left.w * 0.06,
        y: left.y + left.h * 0.04,
        w: left.w * 0.5,
        h: left.h * 0.07,
    }
}

/// The cloth: a square near the top of the right page.
fn cloth_rect(right: UiRect) -> UiRect {
    let side = (right.w * 0.82).min(right.h * 0.6);
    UiRect {
        x: right.x + (right.w - side) * 0.5,
        y: right.y + right.h * 0.04,
        w: side,
        h: side,
    }
}

/// Where `slot` lies in `area` (the cloth, or a card's little drawing).
fn slot_rect(area: UiRect, slot: &BlueprintSlot) -> UiRect {
    let side = slot.size * area.w;
    UiRect {
        x: area.x + slot.at[0] * area.w - side * 0.5,
        y: area.y + slot.at[1] * area.h - side * 0.5,
        w: side,
        h: side,
    }
}

/// Material slot `index` in the strip under the cloth.
fn material_rect(right: UiRect, index: usize) -> UiRect {
    let strip_x = right.x + right.w * 0.06;
    let w = right.w * 0.88 / MATERIAL_SLOTS as f32;
    UiRect {
        x: strip_x + w * index as f32,
        y: right.y + right.h * 0.68,
        w,
        h: w,
    }
}

/// The Craft button, bottom centre of the right page.
fn craft_rect(right: UiRect) -> UiRect {
    let w = right.w * 0.36;
    let h = right.h * 0.08;
    UiRect {
        x: right.x + (right.w - w) * 0.5,
        y: right.y + right.h * 0.86,
        w,
        h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spall_protocol::{InventoryEntry, ProgressionOutcome, ProgressionResponse};

    const LOG: u16 = 1;
    const STONE: u16 = 3;
    const BLOCK: u16 = 4;
    const STICK: u16 = 5;
    const PIT: u16 = 6;
    const SCREEN: [f32; 2] = [1600.0, 900.0];

    #[derive(Debug)]
    struct TestView(Vec<Blueprint>);

    impl PackView for TestView {
        fn catalog_version(&self) -> u32 {
            1
        }
        fn item_name(&self, item_id: u16) -> Option<&str> {
            Some(match item_id {
                LOG => "Log",
                STONE => "Stone",
                BLOCK => "Stone block",
                STICK => "Stick",
                PIT => "Fire pit",
                _ => return None,
            })
        }
        fn item_icon_png(&self, _: u16) -> Option<&'static [u8]> {
            None
        }
        fn blueprints(&self) -> &[Blueprint] {
            &self.0
        }
    }

    fn ring(item: u16, count: usize, radius: f32, size: f32) -> Vec<BlueprintSlot> {
        (0..count)
            .map(|i| {
                let a = i as f32 / count as f32 * std::f32::consts::TAU;
                BlueprintSlot {
                    item,
                    at: [0.5 + radius * a.cos(), 0.5 + radius * a.sin()],
                    size,
                }
            })
            .collect()
    }

    /// A fire pit from above (six stones round six sticks) and a stone block
    /// from the side (two stones on two).
    fn view() -> TestView {
        let mut pit = ring(STONE, 6, 0.34, 0.16);
        pit.extend(ring(STICK, 6, 0.09, 0.12));
        let stone = |x: f32, y: f32| BlueprintSlot {
            item: STONE,
            at: [x, y],
            size: 0.2,
        };
        TestView(vec![
            Blueprint {
                recipe_id: 4,
                output: (PIT, 1),
                view: BlueprintView::Overhead,
                slots: pit,
            },
            Blueprint {
                recipe_id: 2,
                output: (BLOCK, 1),
                view: BlueprintView::Side,
                slots: vec![
                    stone(0.4, 0.64),
                    stone(0.6, 0.64),
                    stone(0.4, 0.44),
                    stone(0.6, 0.44),
                ],
            },
        ])
    }

    fn feed(stacks: &[(u16, u32)]) -> InventoryFeed {
        let mut feed = InventoryFeed::new();
        feed.take_reply(ProgressionResponse {
            request_id: 1,
            catalog_version: 1,
            inventory_revision: 5,
            outcome: ProgressionOutcome::Inventory,
            inventory: stacks
                .iter()
                .map(|&(item_id, count)| InventoryEntry { item_id, count })
                .collect(),
        });
        feed
    }

    fn opened() -> Book {
        let mut book = Book::new();
        book.open();
        book.advance(SLIDE_S);
        book.pointer(SCREEN, [-1.0; 2]);
        book
    }

    fn centre(rect: UiRect) -> [f32; 2] {
        [rect.x + rect.w * 0.5, rect.y + rect.h * 0.5]
    }

    fn click(
        book: &mut Book,
        view: &TestView,
        feed: &InventoryFeed,
        at: [f32; 2],
    ) -> Option<BookAction> {
        book.pointer(SCREEN, at);
        book.press(view, feed);
        book.release(view, feed)
    }

    fn drag(book: &mut Book, view: &TestView, feed: &InventoryFeed, from: [f32; 2], to: [f32; 2]) {
        book.pointer(SCREEN, from);
        book.press(view, feed);
        book.pointer(SCREEN, to);
        assert_eq!(book.release(view, feed), None);
    }

    fn cloth(book: &Book, view: &TestView, feed: &InventoryFeed) -> ClothPage {
        match book.frame(view, feed).unwrap().page {
            BookPage::Cloth(page) => *page,
            other => panic!("not on the cloth: {other:?}"),
        }
    }

    fn open_blueprint(
        book: &mut Book,
        view: &TestView,
        feed: &InventoryFeed,
        index: usize,
    ) -> ClothPage {
        let BookPage::Contents { cards, .. } = book.frame(view, feed).unwrap().page else {
            panic!("on the contents")
        };
        assert_eq!(click(book, view, feed, centre(cards[index].rect)), None);
        cloth(book, view, feed)
    }

    fn material(page: &ClothPage, item: u16) -> [f32; 2] {
        centre(
            page.materials
                .iter()
                .find(|m| m.item.item_id == item)
                .unwrap()
                .rect,
        )
    }

    #[test]
    fn the_contents_show_each_blueprints_layout_and_needs_against_the_pack() {
        let view = view();
        let feed = feed(&[(STONE, 3), (STICK, 7)]);
        let book = opened();
        let frame = book.frame(&view, &feed).unwrap();
        let BookPage::Contents { cards, note } = frame.page else {
            panic!("opens at the contents")
        };
        assert_eq!(note, None);
        assert_eq!(cards[0].name, "Fire pit");
        assert_eq!(
            cards[0].needs,
            vec![
                NeedLine {
                    name: "Stone".into(),
                    need: 6,
                    have: 3
                },
                NeedLine {
                    name: "Stick".into(),
                    need: 6,
                    have: 7
                },
            ]
        );
        assert_eq!(cards[0].pattern.len(), 12);
        assert_eq!(cards[1].view, BlueprintView::Side);
        for card in &cards {
            for (rect, _) in &card.pattern {
                let d = card.diagram;
                assert!(
                    rect.x >= d.x - 1.0 && rect.x + rect.w <= d.x + d.w + 1.0,
                    "drawn in the diagram"
                );
            }
        }
    }

    #[test]
    fn a_material_dropped_near_a_slot_that_takes_it_snaps_in() {
        let view = view();
        let feed = feed(&[(STONE, 6), (STICK, 6)]);
        let mut book = opened();
        let page = open_blueprint(&mut book, &view, &feed, 0);
        let stone_slot = page.slots[0].rect;
        // Dropped a little off the slot's centre: it snaps in.
        let near = [
            centre(stone_slot)[0] + stone_slot.w * 0.4,
            centre(stone_slot)[1],
        ];
        drag(&mut book, &view, &feed, material(&page, STONE), near);
        let page = cloth(&book, &view, &feed);
        assert!(page.slots[0].filled);
        // A stick dropped on a stone slot finds no home: back to the pack.
        drag(
            &mut book,
            &view,
            &feed,
            material(&page, STICK),
            centre(page.slots[1].rect),
        );
        let page = cloth(&book, &view, &feed);
        assert_eq!(page.slots.iter().filter(|s| s.filled).count(), 1);
        // A stick dropped in the middle snaps to one of the centre slots.
        drag(
            &mut book,
            &view,
            &feed,
            material(&page, STICK),
            centre(page.cloth),
        );
        let page = cloth(&book, &view, &feed);
        let sticks = page
            .slots
            .iter()
            .filter(|s| s.filled && s.item.item_id == STICK)
            .count();
        assert_eq!(sticks, 1);
        // Far off the cloth, nothing snaps.
        drag(&mut book, &view, &feed, material(&page, STONE), [5.0, 5.0]);
        let page = cloth(&book, &view, &feed);
        assert_eq!(page.slots.iter().filter(|s| s.filled).count(), 2);
    }

    #[test]
    fn filling_every_slot_readies_craft_and_a_success_clears_the_cloth() {
        let view = view();
        let mut feed = feed(&[(STONE, 6), (STICK, 6)]);
        let mut book = opened();
        let page = open_blueprint(&mut book, &view, &feed, 0);
        assert!(!page.ready);
        let slots = page.slots.clone();
        for (i, slot) in slots.iter().enumerate() {
            drag(
                &mut book,
                &view,
                &feed,
                material(&page, slot.item.item_id),
                centre(slot.rect),
            );
            if i + 1 < slots.len() {
                assert_eq!(
                    click(&mut book, &view, &feed, centre(page.craft)),
                    None,
                    "not yet"
                );
            }
        }
        let page = cloth(&book, &view, &feed);
        assert!(page.slots.iter().all(|s| s.filled), "{:?}", page.slots);
        assert!(page.ready);
        assert!(
            page.materials.iter().all(|m| m.item.count == 0),
            "all laid out"
        );
        assert_eq!(
            click(&mut book, &view, &feed, centre(page.craft)),
            Some(BookAction::Craft(4))
        );
        let request = feed.craft(4, 1);
        let (outcome, _) = feed.take_reply(ProgressionResponse {
            request_id: request.request_id,
            catalog_version: 1,
            inventory_revision: 6,
            outcome: ProgressionOutcome::Crafted,
            inventory: vec![InventoryEntry {
                item_id: PIT,
                count: 1,
            }],
        });
        book.crafted(&outcome.unwrap(), &view);
        book.reconcile(&view, &feed);
        let page = cloth(&book, &view, &feed);
        assert!(page.slots.iter().all(|s| !s.filled), "the cloth is cleared");
        assert_eq!(page.note.as_deref(), Some("Made 1 Fire pit"));
    }

    #[test]
    fn materials_on_the_cloth_never_exceed_the_pack() {
        let view = view();
        let feed = feed(&[(STONE, 2)]);
        let mut book = opened();
        let page = open_blueprint(&mut book, &view, &feed, 1);
        assert_eq!(page.view, BlueprintView::Side);
        for slot in page.slots.clone() {
            drag(
                &mut book,
                &view,
                &feed,
                material(&page, STONE),
                centre(slot.rect),
            );
        }
        let page = cloth(&book, &view, &feed);
        assert_eq!(page.slots.iter().filter(|s| s.filled).count(), 2);
        assert!(!page.ready);
        assert_eq!(click(&mut book, &view, &feed, centre(page.craft)), None);
    }

    #[test]
    fn a_laid_material_clicked_goes_back_and_dragged_moves() {
        let view = view();
        let feed = feed(&[(STONE, 1)]);
        let mut book = opened();
        let page = open_blueprint(&mut book, &view, &feed, 1);
        drag(
            &mut book,
            &view,
            &feed,
            material(&page, STONE),
            centre(page.slots[0].rect),
        );
        drag(
            &mut book,
            &view,
            &feed,
            centre(page.slots[0].rect),
            centre(page.slots[3].rect),
        );
        let page = cloth(&book, &view, &feed);
        assert!(!page.slots[0].filled && page.slots[3].filled, "moved");
        click(&mut book, &view, &feed, centre(page.slots[3].rect));
        let page = cloth(&book, &view, &feed);
        assert!(page.slots.iter().all(|s| !s.filled), "clicked back");
        assert_eq!(page.materials[0].item.count, 1);

        click(&mut book, &view, &feed, centre(page.back));
        assert!(matches!(
            book.frame(&view, &feed).unwrap().page,
            BookPage::Contents { .. }
        ));
    }

    #[test]
    fn while_dragging_the_slot_it_would_snap_into_is_marked() {
        let view = view();
        let feed = feed(&[(STONE, 1)]);
        let mut book = opened();
        let page = open_blueprint(&mut book, &view, &feed, 1);
        book.pointer(SCREEN, material(&page, STONE));
        book.press(&view, &feed);
        book.pointer(SCREEN, centre(page.slots[2].rect));
        let page = cloth(&book, &view, &feed);
        assert_eq!(
            page.slots.iter().map(|s| s.snap).collect::<Vec<_>>(),
            [false, false, true, false]
        );
        assert!(page.dragging.is_some());
    }

    #[test]
    fn a_rejected_craft_keeps_the_cloth_and_says_why() {
        let view = view();
        let feed = feed(&[(STONE, 1)]);
        let mut book = opened();
        let page = open_blueprint(&mut book, &view, &feed, 1);
        drag(
            &mut book,
            &view,
            &feed,
            material(&page, STONE),
            centre(page.slots[1].rect),
        );
        book.crafted(
            &CraftOutcome::Rejected(spall_protocol::ProgressionRejectCode::InsufficientItems),
            &view,
        );
        let page = cloth(&book, &view, &feed);
        assert!(page.slots[1].filled);
        assert_eq!(page.note.as_deref(), Some("Not enough materials"));
    }
}
