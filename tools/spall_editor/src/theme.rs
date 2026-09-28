//! Visual theme and styled widgets for the editor, following the "Spall Editor"
//! design mockup: charcoal surfaces, a single orange accent, flat panels with
//! 1px separators, and dense tabbed inspectors.
//!
//! Yakui only ships Roboto Regular, so weight and hierarchy come from size and
//! color rather than bold/mono faces, and symbols are drawn as shapes instead
//! of relying on glyph coverage.

use yakui::Border;
use yakui::style::{TextAlignment, TextStyle};
use yakui::widgets::{Button, DynamicButtonStyle, Layer, List, Pad, RoundRect, TextBox};
use yakui::{Alignment, Color, Constraints, CrossAxisAlignment, Dim2, MainAxisSize, Pivot, Vec2};

pub const BG_APP: Color = Color::hex(0x0d0e10);
pub const BG_TOPBAR: Color = Color::hex(0x1c1f25);
pub const BG_PANEL: Color = Color::hex(0x1b1e24);
pub const BG_LIBRARY: Color = Color::hex(0x181b21);
pub const BG_RAISED: Color = Color::hex(0x20242b);
pub const BG_HOVER: Color = Color::hex(0x2b3038);
pub const BG_INPUT: Color = Color::hex(0x14161a);
pub const BG_BUTTON: Color = Color::hex(0x262a31);
pub const BG_BUTTON_HOVER: Color = Color::hex(0x2f343c);

pub const LINE: Color = Color::hex(0x262a31);
pub const LINE_CONTROL: Color = Color::hex(0x383e47);
pub const LINE_INPUT: Color = Color::hex(0x34393f);
pub const LINE_CARD: Color = Color::hex(0x2e333b);
pub const LINE_MENU: Color = Color::hex(0x333941);

pub const TEXT: Color = Color::hex(0xe6e8eb);
pub const TEXT_BUTTON: Color = Color::hex(0xd6d9dd);
pub const TEXT_MENU: Color = Color::hex(0xc7cad0);
pub const TEXT_LABEL: Color = Color::hex(0x8b919a);
pub const TEXT_MUTED: Color = Color::hex(0x7b828c);
pub const TEXT_DIM: Color = Color::hex(0x5b6169);
pub const TEXT_DANGER: Color = Color::hex(0xe08484);
/// At-a-glance state colours for On/Off menu entries.
pub const STATE_ON: Color = Color::hex(0x8fe3a1);
pub const STATE_OFF: Color = Color::hex(0xf29b9b);

pub const ACCENT: Color = Color::hex(0xff8a4a);
pub const ACCENT_HOVER: Color = Color::hex(0xffab7a);
pub const ACCENT_TEXT: Color = Color::hex(0x1a1206);

/// Fixed chrome sizes in logical pixels. Pointer hit-testing in `main.rs`
/// derives the viewport rectangle from these, so change them in one place.
pub const TOP_BAR_HEIGHT: f32 = 44.0;
pub const STATUS_BAR_HEIGHT: f32 = 24.0;
pub const PANEL_MIN_WIDTH: f32 = 220.0;

pub fn text_style(size: f32, color: Color) -> TextStyle {
    TextStyle::label().font_size(size).color(color)
}

/// Vertical stack that hugs its content and stretches children to its width.
/// Yakui lists fill their main axis by default, which would make nested
/// stacks swallow the space of their siblings.
pub fn vstack(spacing: f32, children: impl FnOnce()) {
    yakui::util::widget_children::<VStackWidget, _>(children, VStack { spacing });
}

/// Column layout: children are given the full available width and their
/// natural height, and the stack is exactly as tall as its content. Unlike a
/// stretching `List` it never flexes along the main axis, so it is safe inside
/// scroll areas and next to siblings.
#[derive(Debug, Clone)]
struct VStack {
    spacing: f32,
}

#[derive(Debug)]
struct VStackWidget {
    props: VStack,
}

impl yakui::widget::Widget for VStackWidget {
    type Props<'a> = VStack;
    type Response = ();

    fn new() -> Self {
        Self {
            props: VStack { spacing: 0.0 },
        }
    }

    fn update(&mut self, props: Self::Props<'_>) -> Self::Response {
        self.props = props;
    }

    fn layout(&self, mut ctx: yakui::widget::LayoutContext<'_>, input: Constraints) -> Vec2 {
        let node = ctx.dom.get_current();
        let width = input.max.x.is_finite().then_some(input.max.x);
        let child_constraints = Constraints {
            min: Vec2::new(width.unwrap_or(0.0), 0.0),
            max: Vec2::new(width.unwrap_or(f32::INFINITY), f32::INFINITY),
        };
        let mut y = 0.0_f32;
        let mut widest = 0.0_f32;
        for &child in &node.children {
            let size = ctx.calculate_layout(child, child_constraints);
            ctx.layout.set_pos(child, Vec2::new(0.0, y));
            y += size.y + self.props.spacing;
            widest = widest.max(size.x);
        }
        if !node.children.is_empty() {
            y -= self.props.spacing;
        }
        input.constrain(Vec2::new(width.unwrap_or(widest), y))
    }
}

/// Horizontal stack that hugs its content and centers children vertically.
pub fn hstack(spacing: f32, children: impl FnOnce()) {
    List::row()
        .item_spacing(spacing)
        .main_axis_size(MainAxisSize::Min)
        .cross_axis_alignment(CrossAxisAlignment::Center)
        .show(children);
}

/// Yakui clips text to its layout box, which is the exact fractional line
/// width, so the last pixel of a glyph ("On", "Help") can be cut off. A
/// trailing no-break space widens the box enough to keep the ink inside;
/// `centered` adds a leading one so centered text stays visually centered.
fn unclipped(text: impl Into<String>, centered: bool) -> String {
    const GUARD: char = '\u{00A0}';
    let text = text.into();
    let mut out = String::with_capacity(text.len() + 2 * GUARD.len_utf8());
    if centered {
        out.push(GUARD);
    }
    out.push_str(&text);
    out.push(GUARD);
    out
}

pub fn label_centered(size: f32, color: Color, text: impl Into<String>) {
    yakui::widgets::Text::new(size, unclipped(text, true))
        .style(text_style(size, color).align(TextAlignment::Center))
        .show();
}

pub fn label(size: f32, color: Color, text: impl Into<String>) {
    yakui::widgets::Text::new(size, unclipped(text, false))
        .style(text_style(size, color))
        .show();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonKind {
    Primary,
    Secondary,
    Ghost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonSize {
    /// Full-width panel action.
    Regular,
    /// Top-bar and inline actions.
    Small,
    /// Library card actions.
    Tiny,
}

impl ButtonSize {
    fn font(self) -> f32 {
        match self {
            Self::Regular => 12.5,
            Self::Small => 12.0,
            Self::Tiny => 11.0,
        }
    }

    fn padding(self) -> Pad {
        match self {
            Self::Regular => Pad::balanced(12.0, 9.0),
            Self::Small => Pad::balanced(10.0, 5.0),
            Self::Tiny => Pad::balanced(8.0, 5.0),
        }
    }

    fn radius(self) -> f32 {
        match self {
            Self::Regular => 5.0,
            Self::Small | Self::Tiny => 4.0,
        }
    }
}

fn button_state(fill: Color, border: Option<Border>, text: TextStyle) -> DynamicButtonStyle {
    DynamicButtonStyle { text, fill, border }
}

/// A themed button. Returns `true` on the frame it is clicked.
pub fn button(kind: ButtonKind, size: ButtonSize, text: &str) -> bool {
    button_enabled(kind, size, text, true)
}

/// Like [`button`], but a disabled button is dimmed and never reports a click.
pub fn button_enabled(kind: ButtonKind, size: ButtonSize, text: &str, enabled: bool) -> bool {
    let (fill, hover, edge, hover_edge, ink) = match kind {
        ButtonKind::Primary => (
            ACCENT,
            ACCENT_HOVER,
            Some(ACCENT),
            Some(ACCENT_HOVER),
            ACCENT_TEXT,
        ),
        ButtonKind::Secondary => (
            BG_BUTTON,
            BG_BUTTON_HOVER,
            Some(LINE_CONTROL),
            Some(LINE_CONTROL),
            TEXT_BUTTON,
        ),
        ButtonKind::Ghost => (Color::CLEAR, BG_HOVER, None, None, TEXT_MENU),
    };
    let (fill, hover, edge, hover_edge, ink) = if enabled {
        (fill, hover, edge, hover_edge, ink)
    } else {
        (BG_INPUT, BG_INPUT, Some(LINE), Some(LINE), TEXT_DIM)
    };
    let border = |color: Option<Color>| color.map(|color| Border::new(color, 1.0));
    let text_style = text_style(size.font(), ink).align(TextAlignment::Center);
    let mut widget = Button::unstyled(unclipped(text, true));
    widget.padding = size.padding();
    widget.border_radius = size.radius().into();
    widget.style = button_state(fill, border(edge), text_style.clone());
    widget.hover_style = button_state(hover, border(hover_edge), text_style.clone());
    widget.down_style = widget.hover_style.clone();
    widget.focus_style = widget.style.clone();
    widget.show().clicked && enabled
}

pub fn primary(text: &str) -> bool {
    button(ButtonKind::Primary, ButtonSize::Regular, text)
}

pub fn secondary(text: &str) -> bool {
    button(ButtonKind::Secondary, ButtonSize::Regular, text)
}

/// A themed single-line text input. Returns the new text when it changed.
///
/// Yakui text boxes cannot draw a border, so the input is a 1px rounded rect
/// in the border color wrapped around the filled text box.
pub fn input(value: &str, placeholder: &str) -> Option<String> {
    let mut edited = None;
    RoundRect::new(4.0).color(LINE_INPUT).show_children(|| {
        yakui::pad(Pad::all(1.0), || {
            let mut textbox = TextBox::new(value.to_owned());
            textbox.style = text_style(12.0, TEXT).align(TextAlignment::Start);
            textbox.padding = Pad::balanced(9.0, 6.0);
            textbox.fill = Some(BG_INPUT);
            textbox.radius = 3.0;
            textbox.cursor_color = ACCENT;
            textbox.selected_bg_color = ACCENT.with_alpha(0.35);
            textbox.selection_halo_color = ACCENT;
            textbox.placeholder = placeholder.to_owned();
            edited = textbox.show().text.clone();
        });
    });
    edited
}

/// A label above a full-width input.
pub fn field(caption: &str, value: &mut String) {
    field_with_placeholder(caption, value, "");
}

/// Like [`field`] but with a placeholder for the empty state.
pub fn field_with_placeholder(caption: &str, value: &mut String, placeholder: &str) {
    vstack(5.0, || {
        label(11.0, TEXT_LABEL, caption);
        if let Some(text) = input(value, placeholder) {
            *value = text;
        }
    });
}

/// A label above three side-by-side inputs (X/Y/Z, RGB, ...).
pub fn triple(caption: &str, values: &mut [String; 3]) {
    vstack(5.0, || {
        label(11.0, TEXT_LABEL, caption);
        List::row().item_spacing(6.0).show(|| {
            for value in values.iter_mut() {
                yakui::expanded(|| {
                    if let Some(text) = input(value, "") {
                        *value = text;
                    }
                });
            }
        });
    });
}

/// Uppercase muted section heading used inside panels.
pub fn section_title(text: &str) {
    label(11.0, TEXT_MUTED, text.to_uppercase());
}

/// Muted explanatory paragraph.
pub fn hint(text: &str) {
    label(11.5, TEXT_MUTED, text);
}

/// Checkbox with a trailing caption. Returns the new state when toggled.
pub fn checkbox(caption: &str, checked: bool) -> Option<bool> {
    let ink = if checked { TEXT } else { TEXT_MENU };
    let style = text_style(12.0, ink).align(TextAlignment::Start);
    let mut widget = Button::unstyled(String::new());
    widget.padding = Pad::ZERO;
    widget.border_radius = 4.0.into();
    widget.style = button_state(Color::CLEAR, None, style.clone());
    widget.hover_style = button_state(BG_HOVER, None, style.clone());
    widget.down_style = widget.hover_style.clone();
    widget.focus_style = widget.style.clone();
    let mut clicked = false;
    yakui::stack(|| {
        clicked = widget.show().clicked;
        yakui::pad(Pad::balanced(6.0, 4.0), || {
            hstack(8.0, || {
                let (edge, fill) = if checked {
                    (ACCENT, ACCENT)
                } else {
                    (LINE_CONTROL, BG_INPUT)
                };
                RoundRect::new(3.0).color(edge).show_children(|| {
                    yakui::pad(Pad::all(2.0), || {
                        RoundRect::new(2.0)
                            .color(fill)
                            .min_size(Vec2::splat(10.0))
                            .show();
                    });
                });
                label(12.0, ink, caption);
            });
        });
    });
    clicked.then_some(!checked)
}

/// Draggable panel divider. `vertical` draws a vertical line (between
/// side-by-side panels); otherwise a horizontal one. Highlights while
/// hovered or dragged so the resize affordance is discoverable.
pub fn resize_handle(vertical: bool, hot: bool) {
    let color = if hot { ACCENT } else { LINE };
    let size = match (vertical, hot) {
        (true, true) => Vec2::new(3.0, 0.0),
        (true, false) => Vec2::new(1.0, 0.0),
        (false, true) => Vec2::new(0.0, 3.0),
        (false, false) => Vec2::new(0.0, 1.0),
    };
    yakui::colored_box(color, size);
}

/// Top-bar tab for a workspace that only exists while its document is open.
/// Drawn as an outlined pill, accent-coloured when active, so it reads
/// differently from the permanent File/Edit/View/Help menus.
pub fn workspace_tab(text: &str, active: bool) -> bool {
    let (fill, ink, edge) = if active {
        (BG_RAISED, ACCENT, ACCENT)
    } else {
        (Color::CLEAR, TEXT_MUTED, LINE_CONTROL)
    };
    let style = text_style(12.0, ink).align(TextAlignment::Center);
    let border = Some(Border::new(edge, 1.0));
    let mut widget = Button::unstyled(unclipped(text, true));
    widget.padding = Pad::balanced(12.0, 5.0);
    widget.border_radius = 12.0.into();
    widget.style = button_state(fill, border, style.clone());
    widget.hover_style = button_state(BG_HOVER, border, style.clone());
    widget.down_style = widget.hover_style.clone();
    widget.focus_style = widget.style.clone();
    widget.show().clicked
}

/// Modal dialog centred over the whole window. `viewport` is the window size
/// in logical pixels. A dim backdrop layer swallows clicks aimed at the UI
/// underneath; the dialog card sits in a second layer above it. Returns
/// `true` when the close button is clicked.
pub fn modal(viewport: Vec2, title: &str, width: f32, body: impl FnOnce()) -> bool {
    let mut close = false;
    Layer::new().show(|| {
        yakui::constrained(Constraints::tight(viewport), || {
            let dim = Color::rgba(0, 0, 0, 150);
            let style = text_style(12.0, TEXT).align(TextAlignment::Center);
            let mut backdrop = Button::unstyled(String::new());
            backdrop.padding = Pad::ZERO;
            backdrop.style = button_state(dim, None, style.clone());
            backdrop.hover_style = button_state(dim, None, style.clone());
            backdrop.down_style = button_state(dim, None, style.clone());
            backdrop.focus_style = backdrop.style.clone();
            backdrop.show();
        });
    });
    Layer::new().show(|| {
        yakui::constrained(Constraints::tight(viewport), || {
            yakui::align(Alignment::CENTER, || {
                yakui::constrained(Constraints::width(width, width), || {
                    RoundRect::new(9.0).color(LINE_CONTROL).show_children(|| {
                        yakui::pad(Pad::all(1.0), || {
                            RoundRect::new(8.0).color(BG_PANEL).show_children(|| {
                                yakui::pad(Pad::all(20.0), || {
                                    List::column()
                                        .item_spacing(14.0)
                                        .cross_axis_alignment(CrossAxisAlignment::Stretch)
                                        .show(|| {
                                            List::row()
                                                .cross_axis_alignment(CrossAxisAlignment::Center)
                                                .show(|| {
                                                    label(14.0, TEXT, title);
                                                    yakui::expanded(|| {});
                                                    if button(
                                                        ButtonKind::Ghost,
                                                        ButtonSize::Small,
                                                        "Close",
                                                    ) {
                                                        close = true;
                                                    }
                                                });
                                            hline(LINE);
                                            body();
                                        });
                                });
                            });
                        });
                    });
                });
            });
        });
    });
    close
}

pub fn hline(color: Color) {
    yakui::colored_box(color, Vec2::new(0.0, 1.0));
}

pub fn vline(color: Color) {
    yakui::colored_box(color, Vec2::new(1.0, 0.0));
}

/// Pad a panel body with the mockup's standard 16px/14px inset and a
/// consistent vertical rhythm.
pub fn panel_body(spacing: f32, children: impl FnOnce()) {
    yakui::pad(Pad::balanced(16.0, 14.0), || vstack(spacing, children));
}

/// Panel section separated from the next one by a hairline.
pub fn section(children: impl FnOnce()) {
    vstack(0.0, || {
        panel_body(10.0, children);
        hline(LINE);
    });
}

/// Row of tabs with an accent underline on the active one. Returns the index
/// of a newly clicked tab.
pub fn tabs(labels: &[&str], active: usize) -> Option<usize> {
    let mut clicked = None;
    vstack(0.0, || {
        List::row().main_axis_size(MainAxisSize::Max).show(|| {
            for (index, caption) in labels.iter().enumerate() {
                let selected = index == active;
                yakui::expanded(|| {
                    vstack(0.0, || {
                        let (fill, hover, ink) = if selected {
                            (BG_RAISED, BG_RAISED, ACCENT)
                        } else {
                            (Color::CLEAR, BG_RAISED, TEXT_MUTED)
                        };
                        let style = text_style(11.5, ink).align(TextAlignment::Center);
                        let mut widget = Button::unstyled(unclipped(caption.to_uppercase(), true));
                        widget.padding = Pad::balanced(0.0, 10.0);
                        widget.style = button_state(fill, None, style.clone());
                        widget.hover_style = button_state(hover, None, style.clone());
                        widget.down_style = button_state(hover, None, style.clone());
                        widget.focus_style = widget.style.clone();
                        if widget.show().clicked {
                            clicked = Some(index);
                        }
                        yakui::colored_box(
                            if selected { ACCENT } else { LINE },
                            Vec2::new(0.0, 2.0),
                        );
                    });
                });
            }
        });
    });
    clicked
}

/// One entry of a dropdown menu.
pub struct MenuEntry<'a> {
    pub label: &'a str,
    pub shortcut: &'a str,
    pub enabled: bool,
    pub danger: bool,
    /// Draw a separator above this entry.
    pub separator_before: bool,
    /// On/Off state shown at the right edge, light green or light red.
    pub toggle: Option<bool>,
}

impl<'a> MenuEntry<'a> {
    pub fn new(label: &'a str) -> Self {
        Self {
            label,
            shortcut: "",
            enabled: true,
            danger: false,
            separator_before: false,
            toggle: None,
        }
    }

    pub fn shortcut(mut self, shortcut: &'a str) -> Self {
        self.shortcut = shortcut;
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    pub fn danger(mut self) -> Self {
        self.danger = true;
        self
    }

    pub fn toggle(mut self, on: bool) -> Self {
        self.toggle = Some(on);
        self
    }

    pub fn separated(mut self) -> Self {
        self.separator_before = true;
        self
    }
}

/// Which edge of the enclosing widget a dropdown lines up with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropdownSide {
    Left,
    Right,
}

/// Dropdown panel anchored under the enclosing widget, drawn above everything
/// else. Call from inside the widget the menu should hang from. Returns the
/// index of the entry clicked this frame.
pub fn dropdown_from(side: DropdownSide, width: f32, entries: &[&MenuEntry<'_>]) -> Option<usize> {
    let (anchor, pivot) = match side {
        DropdownSide::Left => (Alignment::BOTTOM_LEFT, Pivot::TOP_LEFT),
        DropdownSide::Right => (Alignment::BOTTOM_RIGHT, Pivot::TOP_RIGHT),
    };
    let mut chosen = None;
    yakui::reflow(anchor, pivot, Dim2::pixels(0.0, 4.0), || {
        Layer::new().show(|| {
            yakui::constrained(Constraints::width(width, width), || {
                RoundRect::new(7.0).color(LINE_CONTROL).show_children(|| {
                    yakui::pad(Pad::all(1.0), || {
                        RoundRect::new(6.0).color(BG_RAISED).show_children(|| {
                            yakui::pad(Pad::all(6.0), || {
                                yakui::widgets::List::column()
                                    .cross_axis_alignment(yakui::CrossAxisAlignment::Stretch)
                                    .show(|| {
                                        for (index, entry) in entries.iter().enumerate() {
                                            if entry.separator_before {
                                                yakui::pad(Pad::balanced(4.0, 5.0), || {
                                                    hline(LINE_MENU)
                                                });
                                            }
                                            if menu_row(entry) {
                                                chosen = Some(index);
                                            }
                                        }
                                    });
                            });
                        });
                    });
                });
            });
        });
    });
    chosen
}

/// Top-bar menu title or workspace tab. `active` marks the current workspace
/// or the open menu.
pub fn menu_button(text: &str, active: bool) -> bool {
    let (fill, ink) = if active {
        (BG_RAISED, TEXT)
    } else {
        (Color::CLEAR, TEXT_MENU)
    };
    let style = text_style(12.5, ink).align(TextAlignment::Center);
    let mut widget = Button::unstyled(unclipped(text, true));
    widget.padding = Pad::balanced(10.0, 6.0);
    widget.border_radius = 4.0.into();
    widget.style = button_state(fill, None, style.clone());
    widget.hover_style = button_state(BG_HOVER, None, style.clone());
    widget.down_style = widget.hover_style.clone();
    widget.focus_style = widget.style.clone();
    widget.show().clicked
}

/// Raised button with a leading chevron that opens a dropdown. The chevron
/// sits in a fixed slot at the left, level with the text.
pub fn dropdown_button(text: &str) -> bool {
    const HEIGHT: f32 = 28.0;
    const CHEVRON_SLOT: f32 = 28.0;
    let style = text_style(12.0, TEXT_BUTTON).align(TextAlignment::Start);
    let border = Some(Border::new(LINE_MENU, 1.0));
    let mut widget = Button::unstyled(unclipped(text, false));
    widget.padding = Pad {
        left: CHEVRON_SLOT,
        right: 12.0,
        top: 0.0,
        bottom: 0.0,
    };
    widget.alignment = Alignment::CENTER_LEFT;
    widget.border_radius = 5.0.into();
    widget.style = button_state(BG_RAISED, border, style.clone());
    widget.hover_style = button_state(BG_HOVER, border, style.clone());
    widget.down_style = widget.hover_style.clone();
    widget.focus_style = widget.style.clone();
    let mut clicked = false;
    yakui::stack(|| {
        yakui::constrained(Constraints::height(HEIGHT, HEIGHT), || {
            clicked = widget.show().clicked;
        });
        yakui::constrained(Constraints::tight(Vec2::new(CHEVRON_SLOT, HEIGHT)), || {
            yakui::align(Alignment::CENTER, || chevron_down(TEXT_LABEL));
        });
    });
    clicked
}

const MENU_ROW_HEIGHT: f32 = 30.0;
const SELECT_ROW_HEIGHT: f32 = 32.0;

fn menu_row(entry: &MenuEntry<'_>) -> bool {
    let ink = if !entry.enabled {
        TEXT_DIM
    } else if entry.danger {
        TEXT_DANGER
    } else {
        TEXT
    };
    let mut widget = Button::unstyled(String::new());
    widget.padding = Pad::ZERO;
    widget.border_radius = 5.0.into();
    let plain = button_state(Color::CLEAR, None, text_style(12.5, ink));
    let hover = button_state(
        if entry.enabled {
            BG_HOVER
        } else {
            Color::CLEAR
        },
        None,
        text_style(12.5, ink),
    );
    widget.style = plain;
    widget.hover_style = hover.clone();
    widget.down_style = hover;
    widget.focus_style = widget.style.clone();
    // The button paints the hover fill; the row content is stacked on top and
    // does not intercept the pointer. Both share one fixed height so the
    // highlight and the text stay centered on each other.
    let mut clicked = false;
    yakui::stack(|| {
        yakui::constrained(
            Constraints::height(MENU_ROW_HEIGHT, MENU_ROW_HEIGHT),
            || {
                clicked = widget.show().clicked;
            },
        );
        yakui::constrained(
            Constraints::height(MENU_ROW_HEIGHT, MENU_ROW_HEIGHT),
            || {
                yakui::align(Alignment::CENTER_LEFT, || {
                    yakui::pad(Pad::horizontal(10.0), || {
                        List::row()
                            .cross_axis_alignment(CrossAxisAlignment::Center)
                            .show(|| {
                                yakui::expanded(|| label(12.5, ink, entry.label));
                                if let Some(on) = entry.toggle {
                                    let (word, ink) = if on {
                                        ("On", STATE_ON)
                                    } else {
                                        ("Off", STATE_OFF)
                                    };
                                    label(11.5, ink, word);
                                } else if !entry.shortcut.is_empty() {
                                    label(11.0, TEXT_DIM, entry.shortcut);
                                }
                            });
                    });
                });
            },
        );
    });
    clicked && entry.enabled
}

/// Full-width selectable list row (hierarchy items, layers). The button only
/// paints the fill; stack the row's content on top of it. Returns `true` when
/// clicked.
pub fn select_row(active: bool) -> bool {
    let (fill, ink) = if active {
        (BG_RAISED, TEXT)
    } else {
        (Color::CLEAR, TEXT_MENU)
    };
    let mut widget = Button::unstyled(String::new());
    widget.padding = Pad::ZERO;
    widget.border_radius = 4.0.into();
    widget.style = button_state(fill, None, text_style(12.0, ink));
    widget.hover_style = button_state(BG_HOVER, None, text_style(12.0, ink));
    widget.down_style = widget.hover_style.clone();
    widget.focus_style = widget.style.clone();
    let mut clicked = false;
    yakui::constrained(
        Constraints::height(SELECT_ROW_HEIGHT, SELECT_ROW_HEIGHT),
        || {
            clicked = widget.show().clicked;
        },
    );
    clicked
}

/// Small downward chevron drawn as two strokes.
pub fn chevron_down(color: Color) {
    yakui::constrained(Constraints::tight(Vec2::new(9.0, 12.0)), || {
        yakui::canvas(move |ctx| {
            let widget = ctx.dom.current();
            let Some(rect) = ctx.layout.get(widget).map(|layout| layout.rect) else {
                return;
            };
            let center = rect.center();
            let mut vertices = Vec::new();
            let mut indices = Vec::new();
            let tint = color.to_linear();
            for (from, to) in [
                (Vec2::new(-3.5, -1.5), Vec2::new(0.0, 2.0)),
                (Vec2::new(0.0, 2.0), Vec2::new(3.5, -1.5)),
            ] {
                crate::append_line_quad(
                    &mut vertices,
                    &mut indices,
                    center + from,
                    center + to,
                    tint,
                    1.4,
                );
            }
            ctx.paint
                .add_mesh(yakui::paint::PaintMesh::new(vertices, indices));
        });
    });
}

/// Vector icons drawn on the mockup's 24-unit grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Eye,
    EyeHidden,
}

impl Icon {
    fn strokes(self) -> Vec<(Vec<Vec2>, bool)> {
        let p = |x: f32, y: f32| Vec2::new(x, y);
        match self {
            Self::Eye | Self::EyeHidden => {
                let lens = vec![
                    p(2.0, 12.0),
                    p(6.0, 8.0),
                    p(12.0, 5.0),
                    p(18.0, 8.0),
                    p(22.0, 12.0),
                    p(18.0, 16.0),
                    p(12.0, 19.0),
                    p(6.0, 16.0),
                ];
                let pupil = (0..10)
                    .map(|step| {
                        let radians = std::f32::consts::TAU * step as f32 / 10.0;
                        p(12.0 + 3.0 * radians.cos(), 12.0 + 3.0 * radians.sin())
                    })
                    .collect();
                let mut strokes = vec![(lens, true), (pupil, true)];
                if self == Self::EyeHidden {
                    strokes.push((vec![p(4.0, 20.0), p(20.0, 4.0)], false));
                }
                strokes
            }
        }
    }
}

/// Small borderless icon toggle, such as a layer's visibility eye.
pub fn icon_toggle(icon: Icon, ink: Color) -> bool {
    icon_button(icon, 26.0, 14.0, ink, Color::CLEAR, BG_HOVER)
}

fn icon_button(icon: Icon, tile: f32, glyph: f32, ink: Color, fill: Color, hover: Color) -> bool {
    let mut widget = Button::unstyled(String::new());
    widget.border_radius = 7.0.into();
    widget.style = button_state(fill, None, text_style(12.0, ink));
    widget.hover_style = button_state(hover, None, text_style(12.0, ink));
    widget.down_style = widget.hover_style.clone();
    widget.focus_style = widget.style.clone();
    let mut clicked = false;
    yakui::stack(|| {
        yakui::constrained(Constraints::tight(Vec2::splat(tile)), || {
            clicked = widget.show().clicked;
        });
        yakui::constrained(Constraints::tight(Vec2::splat(tile)), || {
            yakui::canvas(move |ctx| {
                let widget = ctx.dom.current();
                let Some(rect) = ctx.layout.get(widget).map(|layout| layout.rect) else {
                    return;
                };
                let scale = glyph / 24.0;
                let origin = rect.center() - Vec2::splat(12.0 * scale);
                let tint = ink.to_linear();
                let mut vertices = Vec::new();
                let mut indices = Vec::new();
                for (points, closed) in icon.strokes() {
                    let count = points.len();
                    let segments = if closed { count } else { count - 1 };
                    for index in 0..segments {
                        crate::append_line_quad(
                            &mut vertices,
                            &mut indices,
                            origin + points[index] * scale,
                            origin + points[(index + 1) % count] * scale,
                            tint,
                            1.8 * scale + 0.4,
                        );
                    }
                }
                ctx.paint
                    .add_mesh(yakui::paint::PaintMesh::new(vertices, indices));
            });
        });
    });
    clicked
}
