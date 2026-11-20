// SPDX-License-Identifier: GPL-3.0-only

//! The look: the colours (termcode's defaults, which are VS Code's dark theme) and the glyphs
//! of the three icon sets termcode has. Both are plain data a host replaces with its own
//! (`View::set_colors`, `View::set_icons`).
//!
//! Adapted from termcode (src/colors.rs: `to256`, `to16`; src/theme.rs: the icon sets; MIT,
//! the same author). The default colours are VS Code's (Copyright (c) Microsoft Corporation,
//! MIT License), composited over the background they are drawn on, as termcode does.

use ratatui::style::Color;

/// How many colours the terminal shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Depth {
    /// 24-bit colour.
    #[default]
    True,
    C256,
    C16,
}

/// The colours termdisk draws with; the VS Code key each default comes from is noted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Colors {
    /// The treemap and the start screen (`editor.background`).
    pub bg: Color,
    /// The tree pane (`sideBar.background`).
    pub side_bg: Color,
    pub fg: Color,
    /// Dates, notes, item counts (`descriptionForeground`).
    pub dim: Color,
    /// The tree's column headings (`sideBarSectionHeader.*`).
    pub header_bg: Color,
    pub header_fg: Color,
    pub title_bg: Color,
    pub title_fg: Color,
    pub status_bg: Color,
    pub status_fg: Color,
    /// The selected row (`list.activeSelection*`).
    pub sel_bg: Color,
    pub sel_fg: Color,
    /// The mark at the selected row's left edge, the selected tile (`focusBorder`).
    pub focus: Color,
    pub icon_fg: Color,
    /// The tree's levels in turn: the indent guides, and the chevron and folder icon of a row
    /// (`editorBracketHighlight.foreground1` to `3`).
    pub depth: [Color; 3],
    /// The lock on what cannot be trashed (`editorWarning.foreground`).
    pub lock: Color,
    /// Usage bars: the filled part (`button.background`) and the rest (`titleBar.activeBackground`).
    pub bar: Color,
    pub bar_track: Color,
    /// Dialogs and the key list (`editorWidget.*`).
    pub widget_bg: Color,
    pub widget_fg: Color,
    pub widget_border: Color,
    /// The button with the focus (`button.*`), the other (`button.secondary*`), and one that
    /// cannot be used now (`disabledForeground`).
    pub button_bg: Color,
    pub button_fg: Color,
    pub button2_bg: Color,
    pub button2_fg: Color,
    pub disabled: Color,
    /// Treemap tiles, each in the next of these (none blue), and the selected one; white text on
    /// them; the tile of the items too small for their own.
    pub tiles: [Color; 8],
    pub sel_tile: Color,
    pub tile_fg: Color,
    pub rest_bg: Color,
    pub rest_fg: Color,
    /// Under the pointer: a row (`list.hoverBackground`), the buttons
    /// (`button.hoverBackground`, `button.secondaryHoverBackground`), a status bar item
    /// (`statusBarItem.hoverBackground`).
    pub hover_bg: Color,
    pub button_hover: Color,
    pub button2_hover: Color,
    pub status_hover: Color,
    /// The close button at the right of the title bar: its cross, and its background under
    /// the pointer (white cross there, `button_fg`).
    pub close_fg: Color,
    pub close_hover: Color,
    /// The faint lines between the tree's columns.
    pub column_line: Color,
    /// The tree's scrollbar (`scrollbarSlider.background`, opaque: it is drawn as a glyph).
    pub slider: Color,
    /// A ticked row's tint (`list.inactiveSelectionBackground`) and its ticked box
    /// (`testing.iconPassed`); the box also shows without colour.
    pub tick_bg: Color,
    pub tick_fg: Color,
}

const fn rgb(v: u32) -> Color {
    Color::Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

impl Default for Colors {
    fn default() -> Colors {
        Colors {
            bg: rgb(0x1E1E1E),
            side_bg: rgb(0x252526),
            fg: rgb(0xCCCCCC),
            dim: rgb(0x9A9A9A),       // foreground at 0.7 over the side bar
            header_bg: rgb(0x373738), // #80808033 over the side bar
            header_fg: rgb(0xCCCCCC),
            title_bg: rgb(0x3C3C3C),
            title_fg: rgb(0xCCCCCC),
            status_bg: rgb(0x007ACC),
            status_fg: rgb(0xFFFFFF),
            sel_bg: rgb(0x04395E),
            sel_fg: rgb(0xFFFFFF),
            focus: rgb(0x007FD4),
            icon_fg: rgb(0xC5C5C5),
            // Soft gold, orchid and steel blue (xterm's 179, 139, 67).
            depth: [rgb(0xD7AF5F), rgb(0xAF87AF), rgb(0x5F87AF)],
            lock: rgb(0xCCA700),
            bar: rgb(0x0E639C),
            bar_track: rgb(0x3C3C3C),
            widget_bg: rgb(0x252526),
            widget_fg: rgb(0xCCCCCC),
            widget_border: rgb(0x464647), // foreground at 0.2 over the widget
            button_bg: rgb(0x0E639C),
            button_fg: rgb(0xFFFFFF),
            button2_bg: rgb(0x2A2D2E),
            button2_fg: rgb(0xCCCCCC),
            disabled: rgb(0x797979), // foreground at 0.5 over the widget
            // Soft, earthy colours from xterm's 256 (131, 65, 137, 66, 96, 101, 95, 132), dark
            // enough for white text. Blue is in none, so the selected tile is the only blue
            // one; in 16 colours they keep their hue (`vivid16`) and neighbours still differ.
            tiles: [
                rgb(0xAF5F5F),
                rgb(0x5F875F),
                rgb(0xAF875F),
                rgb(0x5F8787),
                rgb(0x875F87),
                rgb(0x87875F),
                rgb(0x875F5F),
                rgb(0xAF5F87),
            ],
            sel_tile: rgb(0x005FD7), // xterm's 26: blue in 16 colours too
            tile_fg: rgb(0xFFFFFF),
            rest_bg: rgb(0x3C3C3C),
            rest_fg: rgb(0xCCCCCC),
            hover_bg: rgb(0x2A2D2E),
            button_hover: rgb(0x1177BB),
            button2_hover: rgb(0x45494E),
            status_hover: rgb(0x1F8AD2), // white at 0.12 over the status bar
            column_line: rgb(0x464647),  // as widget_border
            slider: rgb(0x797979),
            close_fg: rgb(0xF14C4C), // errorForeground
            close_hover: rgb(0xE81123),
            tick_bg: rgb(0x37373D),
            tick_fg: rgb(0x89D185),
        }
    }
}

impl Colors {
    /// These colours as a terminal of depth `d` shows them: the nearest of xterm's 256, or of
    /// the 16 basic ones. Done once, so no frame pays for it.
    pub fn fitted(self, d: Depth) -> Colors {
        let f = |c: Color| match (c, d) {
            (Color::Rgb(r, g, b), Depth::C256) => Color::Indexed(to256(r, g, b)),
            (Color::Rgb(r, g, b), Depth::C16) => to16(r, g, b),
            (c, _) => c,
        };
        // Soft colours that only tell things apart: in 16 colours keep their hue, not grey.
        let v = |c: Color| match (c, d) {
            (Color::Rgb(r, g, b), Depth::C16) => vivid16(r, g, b),
            _ => f(c),
        };
        Colors {
            bg: f(self.bg),
            side_bg: f(self.side_bg),
            fg: f(self.fg),
            dim: f(self.dim),
            header_bg: f(self.header_bg),
            header_fg: f(self.header_fg),
            title_bg: f(self.title_bg),
            title_fg: f(self.title_fg),
            status_bg: f(self.status_bg),
            status_fg: f(self.status_fg),
            sel_bg: f(self.sel_bg),
            sel_fg: f(self.sel_fg),
            focus: f(self.focus),
            icon_fg: f(self.icon_fg),
            depth: self.depth.map(v),
            lock: f(self.lock),
            bar: f(self.bar),
            bar_track: f(self.bar_track),
            widget_bg: f(self.widget_bg),
            widget_fg: f(self.widget_fg),
            widget_border: f(self.widget_border),
            button_bg: f(self.button_bg),
            button_fg: f(self.button_fg),
            button2_bg: f(self.button2_bg),
            button2_fg: f(self.button2_fg),
            disabled: f(self.disabled),
            tiles: self.tiles.map(v),
            sel_tile: f(self.sel_tile),
            tile_fg: f(self.tile_fg),
            rest_bg: f(self.rest_bg),
            rest_fg: f(self.rest_fg),
            hover_bg: f(self.hover_bg),
            button_hover: f(self.button_hover),
            button2_hover: f(self.button2_hover),
            status_hover: f(self.status_hover),
            column_line: f(self.column_line),
            slider: f(self.slider),
            close_fg: f(self.close_fg),
            close_hover: f(self.close_hover),
            tick_bg: f(self.tick_bg),
            tick_fg: f(self.tick_fg),
        }
    }
}

fn dist((r, g, b): (u8, u8, u8), (r2, g2, b2): (u8, u8, u8)) -> i32 {
    let d = |a: u8, b: u8| (a as i32 - b as i32).pow(2);
    d(r, r2) + d(g, g2) + d(b, b2)
}

/// The nearest of xterm's colour cube (16-231) and grey ramp (232-255).
fn to256(r: u8, g: u8, b: u8) -> u8 {
    const STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let step = |v: u8| {
        (0..6)
            .min_by_key(|&i| (STEPS[i] as i32 - v as i32).abs())
            .unwrap()
    };
    let (ri, gi, bi) = (step(r), step(g), step(b));
    let cube = (STEPS[ri], STEPS[gi], STEPS[bi]);
    let avg = (r as i32 + g as i32 + b as i32) / 3;
    let n = ((avg - 8 + 5) / 10).clamp(0, 23);
    let grey = (8 + 10 * n) as u8;
    if dist((r, g, b), (grey, grey, grey)) < dist((r, g, b), cube) {
        232 + n as u8
    } else {
        16 + 36 * ri as u8 + 6 * gi as u8 + bi as u8
    }
}

/// The nearest of the 16 basic colours (xterm's defaults).
fn to16(r: u8, g: u8, b: u8) -> Color {
    const BASIC: [((u8, u8, u8), Color); 16] = [
        ((0, 0, 0), Color::Black),
        ((205, 0, 0), Color::Red),
        ((0, 205, 0), Color::Green),
        ((205, 205, 0), Color::Yellow),
        ((0, 0, 238), Color::Blue),
        ((205, 0, 205), Color::Magenta),
        ((0, 205, 205), Color::Cyan),
        ((229, 229, 229), Color::Gray),
        ((127, 127, 127), Color::DarkGray),
        ((255, 0, 0), Color::LightRed),
        ((0, 255, 0), Color::LightGreen),
        ((255, 255, 0), Color::LightYellow),
        ((92, 92, 255), Color::LightBlue),
        ((255, 0, 255), Color::LightMagenta),
        ((0, 255, 255), Color::LightCyan),
        ((255, 255, 255), Color::White),
    ];
    BASIC
        .iter()
        .min_by_key(|(c, _)| dist((r, g, b), *c))
        .unwrap()
        .1
}

/// The nearest of the 16 basic colours to `(r, g, b)` at full strength, so a muted colour keeps
/// its hue instead of turning grey; a grey stays grey.
fn vivid16(r: u8, g: u8, b: u8) -> Color {
    let (lo, hi) = (r.min(g).min(b), r.max(g).max(b));
    if hi - lo < 16 {
        return to16(r, g, b);
    }
    let full = |v: u8| ((v - lo) as u32 * 255 / (hi - lo) as u32) as u8;
    to16(full(r), full(g), full(b))
}

/// The glyphs termdisk draws with, as termcode's icon sets have them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IconSet {
    /// VS Code's codicons, from a Nerd Font.
    Nerd,
    /// Standard Unicode symbols: any font.
    #[default]
    Unicode,
    /// ASCII only (no East Asian ambiguous widths).
    Ascii,
}

/// One icon set's glyphs. Every one is one cell wide but ASCII's tick boxes; an empty icon is
/// no icon.
pub(crate) struct Glyphs {
    pub closed: &'static str,
    pub open: &'static str,
    pub folder: &'static str,
    pub folder_open: &'static str,
    pub lock: &'static str,
    /// The selected row's left edge, the selected tile's first cell.
    pub mark: &'static str,
    /// Corners (top left, top right, bottom left, bottom right), then across and down.
    pub border: [&'static str; 6],
    /// A usage bar: a whole cell, half a cell, and the track.
    pub bar: [&'static str; 3],
    /// Between the parts of a title, between figures, and where text is cut.
    pub sep: &'static str,
    pub dot: &'static str,
    pub cut: &'static str,
    /// The keys in the hints: up and down, right and left, Backspace.
    pub keys: [&'static str; 3],
    /// A running scan's spinner, one frame per redraw (every 100 ms).
    pub spin: &'static [&'static str],
    /// The tree's scrollbar; a done message.
    pub slider: &'static str,
    pub check: &'static str,
    /// After the heading the tree is sorted by: ascending, descending.
    pub sort: [&'static str; 2],
    /// The first cell of each level of the tree's indent.
    pub guide: &'static str,
    /// The close button's cross.
    pub close: &'static str,
    /// A row's tick box, in its icon's place: not ticked, ticked (or in a ticked folder), not
    /// ticked but holding a ticked item. A click on it ticks the row, or takes the tick off.
    pub boxes: [&'static str; 3],
    /// The gutter row under a map tile, drawn in the tile's colour: its upper half only, so the
    /// gap between rows of tiles looks as wide as the one-column gap between tiles (a cell is
    /// about twice as tall as it is wide). None in ASCII: the whole row is the gap.
    pub half_row: &'static str,
}

const NERD: Glyphs = Glyphs {
    closed: "\u{eab6}",
    open: "\u{eab4}",
    folder: "\u{e5ff}",
    folder_open: "\u{e5fe}",
    // The lock is a safety sign: the font-independent one, even here (a codicon is a blank box
    // where the set is forced without a Nerd Font).
    ..UNICODE
};

const UNICODE: Glyphs = Glyphs {
    closed: "▸",
    open: "▾",
    folder: "",
    folder_open: "",
    lock: "⊘",
    mark: "▌",
    border: ["╭", "╮", "╰", "╯", "─", "│"],
    bar: ["━", "╸", "─"],
    sep: "—",
    dot: "·",
    cut: "…",
    keys: ["↑↓", "→←", "⌫"],
    spin: &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"],
    slider: "┃",
    check: "✓",
    // Not ▲▼: those are two cells wide where East Asian ambiguous widths are wide.
    sort: ["▴", "▾"],
    guide: "│",
    close: "✕", // not ×: that one is East Asian ambiguous
    // Both East Asian width N: one cell everywhere.
    boxes: ["☐", "☑", "◩"],
    half_row: "▀", // a block element, as `mark` is
};

const ASCII: Glyphs = Glyphs {
    closed: ">",
    open: "v",
    folder: "",
    folder_open: "",
    lock: "!",
    mark: ">",
    border: ["+", "+", "+", "+", "-", "|"],
    bar: ["=", "-", " "],
    sep: "-",
    dot: "-",
    cut: "~",
    keys: ["Up/Dn", "Rt/Lt", "Bksp"],
    spin: &["|", "/", "-", "\\"],
    slider: "|",
    check: "+",
    sort: ["^", "v"],
    guide: "|",
    close: "x",
    boxes: ["[ ]", "[x]", "[-]"],
    half_row: "",
};

impl IconSet {
    pub(crate) fn glyphs(self) -> &'static Glyphs {
        match self {
            IconSet::Nerd => &NERD,
            IconSet::Unicode => &UNICODE,
            IconSet::Ascii => &ASCII,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn colours_fit_each_depth() {
        let c = Colors::default();
        assert_eq!(c.fitted(Depth::True), c);
        let c256 = c.fitted(Depth::C256);
        assert_eq!(
            (c256.bg, c256.status_bg),
            (Color::Indexed(234), Color::Indexed(32))
        );
        let c16 = c.fitted(Depth::C16);
        assert_eq!((c16.status_bg, c16.button_bg), (Color::Cyan, Color::Cyan));
        // In 16 colours the selection is as black as the pane: the marks have to show it.
        assert_eq!((c16.sel_bg, c16.side_bg), (Color::Black, Color::Black));
        let all = |c: &Colors| {
            [
                c.bg, c.fg, c.sel_bg, c.focus, c.disabled, c.tiles[3], c.sel_tile, c.hover_bg,
                c.slider,
            ]
        };
        assert!(all(&c256).iter().all(|c| matches!(c, Color::Indexed(_))));
        assert!(all(&c16)
            .iter()
            .all(|c| !matches!(c, Color::Rgb(..) | Color::Indexed(_))));
    }

    #[test]
    fn tiles_differ_from_their_neighbours_and_only_the_selected_one_is_blue() {
        for d in [Depth::True, Depth::C256, Depth::C16] {
            let c = Colors::default().fitted(d);
            let n = c.tiles.len();
            for i in 0..n {
                // The colours go round: the last one is next to the first.
                assert_ne!(c.tiles[i], c.tiles[(i + 1) % n], "{d:?}: {i}");
                assert_ne!(c.tiles[i], c.sel_tile, "{d:?}: {i}");
                assert_ne!(c.tiles[i], c.rest_bg, "{d:?}: {i}");
            }
        }
        let (c256, c16) = (
            Colors::default().fitted(Depth::C256),
            Colors::default().fitted(Depth::C16),
        );
        assert_eq!(c256.sel_tile, Color::Indexed(26));
        let soft: Vec<Color> = [131, 65, 137, 66, 96, 101, 95, 132]
            .map(Color::Indexed)
            .into();
        assert_eq!(c256.tiles.to_vec(), soft);
        assert_eq!(c16.sel_tile, Color::Blue);
        let blue = [Color::Blue, Color::LightBlue];
        assert!(!c16.tiles.iter().any(|t| blue.contains(t)));
        // Nor bluish in true colour: blue is never the strongest part (violet would be).
        for t in Colors::default().tiles {
            let Color::Rgb(r, g, b) = t else {
                panic!("{t:?}")
            };
            assert!(b <= r.max(g), "{t:?}");
        }
    }

    #[test]
    fn neighbouring_levels_of_the_tree_differ_at_every_depth() {
        for d in [Depth::True, Depth::C256, Depth::C16] {
            let c = Colors::default().fitted(d).depth;
            for i in 0..c.len() {
                // The levels go round: the last one is next to the first.
                assert_ne!(c[i], c[(i + 1) % c.len()], "{d:?}: {i}");
            }
        }
        let c16 = Colors::default().fitted(Depth::C16).depth;
        assert_eq!(c16, [Color::Yellow, Color::LightMagenta, Color::Cyan]);
        let c256 = Colors::default().fitted(Depth::C256).depth;
        assert_eq!(c256, [179, 139, 67].map(Color::Indexed));
    }

    #[test]
    fn every_glyph_is_one_cell_or_none_and_ascii_is_ascii() {
        for set in [IconSet::Nerd, IconSet::Unicode, IconSet::Ascii] {
            let g = set.glyphs();
            let one = [
                g.closed, g.open, g.lock, g.mark, g.cut, g.sep, g.dot, g.slider, g.check, g.guide,
                g.close,
            ];
            assert!(one.iter().chain(g.spin).all(|s| s.width() == 1), "{set:?}");
            let none_or_one = [g.folder, g.folder_open, g.half_row];
            assert!(none_or_one.iter().all(|s| s.width() <= 1), "{set:?}");
            // The boxes take the same cells (the names stay put as a tick comes and goes),
            // even where East Asian ambiguous widths are wide; one cell but in ASCII.
            let w = match set {
                IconSet::Ascii => 3,
                _ => 1,
            };
            for b in g.boxes {
                assert_eq!((b.width(), b.width_cjk()), (w, w), "{set:?} {b}");
            }
            assert!(
                g.border
                    .iter()
                    .chain(&g.bar)
                    .chain(&g.sort)
                    .all(|s| s.width() == 1),
                "{set:?}"
            );
        }
        let a = IconSet::Ascii.glyphs();
        let all = [
            a.closed, a.open, a.lock, a.mark, a.sep, a.dot, a.cut, a.slider, a.check, a.guide,
            a.boxes[0], a.boxes[1], a.boxes[2], a.half_row,
        ];
        assert!(all
            .iter()
            .chain(a.spin)
            .chain(&a.border)
            .chain(&a.bar)
            .chain(&a.keys)
            .chain(&a.sort)
            .all(|s| s.is_ascii()));
    }
}
