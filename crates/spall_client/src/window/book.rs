//! The blueprint book (B): an open journal of the game's blueprints, each
//! showing its pattern and how much of each raw material it needs against
//! how much the player has. Clicking one turns to the crafting cloth: the
//! player drags raw materials from the pack onto a 3 by 3 cloth, and once
//! they lie in the blueprint's pattern, Craft asks the server for it. UI
//! only; the world keeps running.

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
/// The crafting cloth's squares on a side.
const CLOTH_SIDE: usize = 3;
const CLOTH_SQUARES: usize = CLOTH_SIDE * CLOTH_SIDE;
/// Material slots along the strip under the cloth.
const MATERIAL_SLOTS: usize = 6;

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
    /// The pattern's squares, each with its item if any.
    pub(super) pattern: Vec<(UiRect, Option<ItemDraw>)>,
    pub(super) needs: Vec<NeedLine>,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct ClothPage {
    pub(super) back: UiRect,
    pub(super) back_hovered: bool,
    /// The chosen blueprint, drawn large on the left page.
    pub(super) blueprint: BlueprintCard,
    pub(super) cloth: UiRect,
    pub(super) squares: Vec<ClothSquare>,
    pub(super) materials: Vec<MaterialSlot>,
    pub(super) craft: UiRect,
    /// The materials lie in the blueprint's pattern.
    pub(super) ready: bool,
    pub(super) craft_hovered: bool,
    /// A craft is waiting on the server.
    pub(super) crafting: bool,
    pub(super) note: Option<String>,
    /// The material being dragged, under the cursor.
    pub(super) dragging: Option<(UiRect, ItemDraw)>,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct ClothSquare {
    pub(super) rect: UiRect,
    pub(super) item: Option<ItemDraw>,
    pub(super) hovered: bool,
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
    Square(usize),
    Craft,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Drag {
    item: u16,
    /// The square it was picked up from, or `None` from the strip.
    from: Option<usize>,
}

pub(super) struct Book {
    open: bool,
    /// 0 out of sight, 1 fully in (linear in time).
    amount: f32,
    /// The blueprint whose cloth is open.
    selected: Option<usize>,
    squares: [Option<u16>; CLOTH_SQUARES],
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
            squares: [None; CLOTH_SQUARES],
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
        self.selected = None;
        self.squares = [None; CLOTH_SQUARES];
        self.drag = None;
        self.pressed = None;
        self.note = None;
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

    /// The cursor moved (logical pixels on a `screen` of logical pixels).
    pub(super) fn pointer(&mut self, screen: [f32; 2], cursor: [f32; 2]) {
        self.screen = screen;
        self.cursor = cursor;
    }

    /// How many of `item_id` lie on the cloth.
    fn placed(&self, item_id: u16) -> u32 {
        self.squares
            .iter()
            .filter(|square| **square == Some(item_id))
            .count() as u32
            + u32::from(self.drag.is_some_and(|drag| drag.item == item_id))
    }

    /// How many of `item_id` are still in the pack, off the cloth.
    fn available(&self, item_id: u16, feed: &InventoryFeed) -> u32 {
        feed.count(item_id).saturating_sub(self.placed(item_id))
    }

    /// Takes back anything on the cloth the inventory no longer has (it was
    /// crafted, or used elsewhere).
    pub(super) fn reconcile(&mut self, feed: &InventoryFeed) {
        if feed.stacks().is_none() {
            return;
        }
        for i in 0..CLOTH_SQUARES {
            if let Some(item) = self.squares[i]
                && self.placed(item) > feed.count(item)
            {
                self.squares[i] = None;
            }
        }
    }

    pub(super) fn press(&mut self, view: &dyn PackView, feed: &InventoryFeed) {
        if !self.open {
            return;
        }
        let target = self.target(view, feed);
        self.pressed = target;
        match target {
            Some(Target::Blueprint(index)) => {
                self.selected = Some(index);
                self.squares = [None; CLOTH_SQUARES];
                self.note = None;
            }
            Some(Target::Back) => {
                self.selected = None;
                self.squares = [None; CLOTH_SQUARES];
                self.note = None;
            }
            Some(Target::Material(item)) if self.available(item, feed) > 0 => {
                self.drag = Some(Drag { item, from: None });
                self.note = None;
            }
            Some(Target::Square(index)) => {
                if let Some(item) = self.squares[index].take() {
                    self.drag = Some(Drag {
                        item,
                        from: Some(index),
                    });
                    self.note = None;
                }
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
            // A material dropped on a bare square stays there; anywhere else
            // (including back where it came from) it returns to the pack.
            if let Some(Target::Square(index)) = target
                && Some(index) != drag.from
                && self.squares[index].is_none()
            {
                self.squares[index] = Some(drag.item);
            }
            return None;
        }
        if pressed == Some(Target::Craft)
            && target == Some(Target::Craft)
            && !feed.crafting()
            && let Some(blueprint) = self.blueprint(view)
            && arranged(blueprint, &self.squares)
        {
            self.note = Some("Crafting\u{2026}".to_owned());
            return Some(BookAction::Craft(blueprint.recipe_id));
        }
        None
    }

    /// The server answered the craft.
    pub(super) fn crafted(&mut self, outcome: &CraftOutcome, view: &dyn PackView) {
        self.note = Some(match outcome {
            CraftOutcome::Crafted { recipe_id } => {
                self.squares = [None; CLOTH_SQUARES];
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

    fn blueprint<'v>(&self, view: &'v dyn PackView) -> Option<&'v Blueprint> {
        view.blueprints().get(self.selected?)
    }

    /// What lies under the cursor in the current layout.
    fn target(&self, view: &dyn PackView, feed: &InventoryFeed) -> Option<Target> {
        let book = book_rect(self.screen, self.eased());
        let at = self.cursor;
        let Some(_) = self.blueprint(view) else {
            return (0..view.blueprints().len().min(CARDS))
                .find(|&i| card_rect(book, i).contains(at))
                .map(Target::Blueprint);
        };
        let (left, right) = pages(book);
        if back_rect(left).contains(at) {
            return Some(Target::Back);
        }
        let cloth = cloth_rect(right);
        if let Some(i) = (0..CLOTH_SQUARES).find(|&i| square_rect(cloth, i).contains(at)) {
            return Some(Target::Square(i));
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
                    h: left.h - (back.y + back.h * 1.4 - left.y) - left.h * 0.06,
                    ..left
                };
                let cloth = cloth_rect(right);
                let cell = square_rect(cloth, 0);
                BookPage::Cloth(Box::new(ClothPage {
                    back,
                    back_hovered: hovered == Some(Target::Back),
                    blueprint: card(view, feed, blueprint, inset(below_back, 0.08), false),
                    cloth,
                    squares: (0..CLOTH_SQUARES)
                        .map(|i| ClothSquare {
                            rect: square_rect(cloth, i),
                            item: self.squares[i].map(|item| draw(item, 0)),
                            hovered: hovered == Some(Target::Square(i)),
                        })
                        .collect(),
                    materials: materials(feed)
                        .enumerate()
                        .map(|(i, item)| MaterialSlot {
                            rect: material_rect(right, i),
                            item: draw(item, self.available(item, feed)),
                            name: view.item_name(item).unwrap_or("Unknown item").to_owned(),
                            hovered: hovered == Some(Target::Material(item)),
                        })
                        .collect(),
                    craft: craft_rect(right),
                    ready: arranged(blueprint, &self.squares),
                    craft_hovered: hovered == Some(Target::Craft),
                    crafting: feed.crafting(),
                    note: self.note.clone(),
                    dragging: self.drag.map(|drag| {
                        let side = cell.w * 0.8;
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
                .chain(card.pattern.iter().filter_map(|(_, item)| item.as_ref()))
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

/// A blueprint drawn in `rect`: name, pattern, needs against the inventory.
fn card(
    view: &dyn PackView,
    feed: &InventoryFeed,
    blueprint: &Blueprint,
    rect: UiRect,
    hovered: bool,
) -> BlueprintCard {
    let (item, count) = blueprint.output;
    let (w, h) = blueprint.size();
    // The pattern sits in the card's left half, below the name.
    let area = UiRect {
        x: rect.x + rect.w * 0.06,
        y: rect.y + rect.h * 0.28,
        w: rect.w * 0.42,
        h: rect.h * 0.64,
    };
    let side = (area.w / w.max(1) as f32).min(area.h / h.max(1) as f32);
    let pattern = (0..h)
        .flat_map(|y| (0..w).map(move |x| (x, y)))
        .map(|(x, y)| {
            let square = UiRect {
                x: area.x + side * x as f32,
                y: area.y + side * y as f32,
                w: side,
                h: side,
            };
            let item = blueprint.at(x, y).map(|item| ItemDraw {
                item_id: item,
                icon_png: view.item_icon_png(item),
                count: 0,
            });
            (square, item)
        })
        .collect();
    BlueprintCard {
        rect,
        hovered,
        name: view.item_name(item).unwrap_or("Unknown item").to_owned(),
        output: ItemDraw {
            item_id: item,
            icon_png: view.item_icon_png(item),
            count,
        },
        pattern,
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

/// Whether `squares` hold exactly the blueprint's pattern, placed anywhere on
/// the cloth, with nothing else on it.
pub(super) fn arranged(blueprint: &Blueprint, squares: &[Option<u16>; CLOTH_SQUARES]) -> bool {
    let (w, h) = blueprint.size();
    if w == 0 || h == 0 || w > CLOTH_SIDE || h > CLOTH_SIDE {
        return false;
    }
    (0..=CLOTH_SIDE - h).any(|dy| {
        (0..=CLOTH_SIDE - w).any(|dx| {
            (0..CLOTH_SQUARES).all(|i| {
                let (x, y) = (i % CLOTH_SIDE, i / CLOTH_SIDE);
                let wanted = (x >= dx && y >= dy)
                    .then(|| blueprint.at(x - dx, y - dy))
                    .flatten();
                squares[i] == wanted
            })
        })
    })
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
    let side = (right.w * 0.74).min(right.h * 0.58);
    UiRect {
        x: right.x + (right.w - side) * 0.5,
        y: right.y + right.h * 0.05,
        w: side,
        h: side,
    }
}

/// Cloth square `index`, inside the cloth's hem, rows from the top.
fn square_rect(cloth: UiRect, index: usize) -> UiRect {
    let inner = inset(cloth, 0.08);
    let side = inner.w / CLOTH_SIDE as f32;
    inset(
        UiRect {
            x: inner.x + side * (index % CLOTH_SIDE) as f32,
            y: inner.y + side * (index / CLOTH_SIDE) as f32,
            w: side,
            h: side,
        },
        0.06,
    )
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
    const PLANK: u16 = 2;
    const STONE: u16 = 3;
    const BLOCK: u16 = 4;
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
                PLANK => "Plank",
                STONE => "Stone",
                BLOCK => "Stone block",
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

    fn view() -> TestView {
        TestView(vec![
            Blueprint {
                recipe_id: 1,
                output: (PLANK, 4),
                pattern: vec![vec![Some(LOG)]],
            },
            Blueprint {
                recipe_id: 2,
                output: (BLOCK, 1),
                pattern: vec![
                    vec![Some(STONE), Some(STONE)],
                    vec![Some(STONE), Some(STONE)],
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

    fn drag(book: &mut Book, view: &TestView, feed: &InventoryFeed, from: UiRect, to: UiRect) {
        book.pointer(SCREEN, centre(from));
        book.press(view, feed);
        book.pointer(SCREEN, centre(to));
        assert_eq!(book.release(view, feed), None);
    }

    fn cloth(book: &Book, view: &TestView, feed: &InventoryFeed) -> ClothPage {
        match book.frame(view, feed).unwrap().page {
            BookPage::Cloth(page) => *page,
            other => panic!("not on the cloth: {other:?}"),
        }
    }

    #[test]
    fn a_pattern_matches_anywhere_on_the_cloth_and_nothing_else_may_lie_there() {
        let view = view();
        let [planks, block] = &view.0[..] else {
            unreachable!()
        };
        let mut squares = [None; CLOTH_SQUARES];
        assert!(!arranged(planks, &squares));
        squares[8] = Some(LOG);
        assert!(arranged(planks, &squares), "one log in any square");
        squares[0] = Some(LOG);
        assert!(!arranged(planks, &squares), "an extra log spoils it");

        let mut squares = [None; CLOTH_SQUARES];
        for i in [4, 5, 7, 8] {
            squares[i] = Some(STONE);
        }
        assert!(
            arranged(block, &squares),
            "a 2 by 2 square in the lower right"
        );
        squares[5] = None;
        squares[3] = Some(STONE);
        assert!(!arranged(block, &squares), "four stones, wrong shape");
        assert_eq!(block.needs(), vec![(STONE, 4)]);
    }

    #[test]
    fn the_contents_show_each_blueprints_needs_against_what_the_player_has() {
        let view = view();
        let feed = feed(&[(STONE, 3)]);
        let book = opened();
        let frame = book.frame(&view, &feed).unwrap();
        let BookPage::Contents { cards, note } = frame.page else {
            panic!("opens at the contents")
        };
        assert_eq!(note, None);
        assert_eq!(cards.len(), 2);
        assert_eq!(
            (cards[0].name.as_str(), cards[0].output.count),
            ("Plank", 4)
        );
        assert_eq!(
            cards[1].needs,
            vec![NeedLine {
                name: "Stone".into(),
                need: 4,
                have: 3
            }]
        );
        assert_eq!(cards[1].pattern.len(), 4, "a 2 by 2 pattern");
        for card in &cards {
            let UiRect { x, y, w, h } = card.rect;
            let b = frame.book;
            assert!(x >= b.x && y >= b.y && x + w <= b.x + b.w && y + h <= b.y + b.h);
        }
    }

    #[test]
    fn laying_stones_in_the_pattern_readies_craft_and_sends_the_recipe() {
        let view = view();
        let mut feed = feed(&[(STONE, 5), (LOG, 1)]);
        let mut book = opened();
        let BookPage::Contents { cards, .. } = book.frame(&view, &feed).unwrap().page else {
            unreachable!()
        };
        assert_eq!(click(&mut book, &view, &feed, centre(cards[1].rect)), None);
        let page = cloth(&book, &view, &feed);
        assert!(!page.ready);
        let stone = page
            .materials
            .iter()
            .find(|m| m.item.item_id == STONE)
            .unwrap()
            .rect;
        for i in [0, 1, 3, 4] {
            drag(&mut book, &view, &feed, stone, page.squares[i].rect);
        }
        let page = cloth(&book, &view, &feed);
        assert!(page.ready);
        let left = page
            .materials
            .iter()
            .find(|m| m.item.item_id == STONE)
            .unwrap();
        assert_eq!(left.item.count, 1, "four of five stones on the cloth");
        // A craft must be a click on the button, not a drop on it.
        assert_eq!(
            click(&mut book, &view, &feed, centre(page.craft)),
            Some(BookAction::Craft(2))
        );
        let request = feed.craft(2, 1);
        let (outcome, _) = feed.take_reply(ProgressionResponse {
            request_id: request.request_id,
            catalog_version: 1,
            inventory_revision: 6,
            outcome: ProgressionOutcome::Crafted,
            inventory: vec![
                InventoryEntry {
                    item_id: STONE,
                    count: 1,
                },
                InventoryEntry {
                    item_id: BLOCK,
                    count: 1,
                },
            ],
        });
        book.crafted(&outcome.unwrap(), &view);
        book.reconcile(&feed);
        let page = cloth(&book, &view, &feed);
        assert!(
            page.squares.iter().all(|s| s.item.is_none()),
            "the cloth is cleared"
        );
        assert_eq!(page.note.as_deref(), Some("Made 1 Stone block"));
    }

    #[test]
    fn craft_waits_for_the_pattern_and_materials_cannot_exceed_the_pack() {
        let view = view();
        let feed = feed(&[(STONE, 2)]);
        let mut book = opened();
        let BookPage::Contents { cards, .. } = book.frame(&view, &feed).unwrap().page else {
            unreachable!()
        };
        click(&mut book, &view, &feed, centre(cards[1].rect));
        let page = cloth(&book, &view, &feed);
        let stone = page.materials[0].rect;
        for i in [0, 1, 3] {
            drag(&mut book, &view, &feed, stone, page.squares[i].rect);
        }
        let page = cloth(&book, &view, &feed);
        let on_cloth = page.squares.iter().filter(|s| s.item.is_some()).count();
        assert_eq!(on_cloth, 2, "only two stones to lay out");
        assert!(!page.ready);
        assert_eq!(click(&mut book, &view, &feed, centre(page.craft)), None);
    }

    #[test]
    fn a_square_clicked_or_dragged_off_gives_its_material_back_and_can_move() {
        let view = view();
        let feed = feed(&[(LOG, 1)]);
        let mut book = opened();
        let BookPage::Contents { cards, .. } = book.frame(&view, &feed).unwrap().page else {
            unreachable!()
        };
        click(&mut book, &view, &feed, centre(cards[0].rect));
        let page = cloth(&book, &view, &feed);
        let log = page.materials[0].rect;
        drag(&mut book, &view, &feed, log, page.squares[0].rect);
        drag(
            &mut book,
            &view,
            &feed,
            page.squares[0].rect,
            page.squares[8].rect,
        );
        let page = cloth(&book, &view, &feed);
        assert!(
            page.squares[0].item.is_none() && page.squares[8].item.is_some(),
            "moved"
        );
        assert!(page.ready, "one log anywhere");
        click(&mut book, &view, &feed, centre(page.squares[8].rect));
        let page = cloth(&book, &view, &feed);
        assert!(
            page.squares.iter().all(|s| s.item.is_none()),
            "clicked back"
        );
        assert_eq!(page.materials[0].item.count, 1);

        click(&mut book, &view, &feed, centre(page.back));
        assert!(matches!(
            book.frame(&view, &feed).unwrap().page,
            BookPage::Contents { .. }
        ));
    }

    #[test]
    fn a_rejected_craft_keeps_the_cloth_and_says_why() {
        let view = view();
        let feed = feed(&[(LOG, 1)]);
        let mut book = opened();
        let BookPage::Contents { cards, .. } = book.frame(&view, &feed).unwrap().page else {
            unreachable!()
        };
        click(&mut book, &view, &feed, centre(cards[0].rect));
        let page = cloth(&book, &view, &feed);
        drag(
            &mut book,
            &view,
            &feed,
            page.materials[0].rect,
            page.squares[4].rect,
        );
        book.crafted(
            &CraftOutcome::Rejected(spall_protocol::ProgressionRejectCode::InsufficientItems),
            &view,
        );
        let page = cloth(&book, &view, &feed);
        assert!(page.squares[4].item.is_some());
        assert_eq!(page.note.as_deref(), Some("Not enough materials"));
    }
}
