// SPDX-License-Identifier: GPL-3.0-only

//! Drawing the view into a ratatui `Buffer`: cells, colours and text, nothing else.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use unicode_width::UnicodeWidthChar;

use crate::guard::Policy;
use crate::style::{Colors, Glyphs};
use crate::sys;
use crate::tree::{Tree, HARDLINK_DUP, LINKED, MOUNT, NOT_OWNED, PROTECTED, UNREADABLE};
use crate::treemap::{self, Item};
use crate::view::{counted, Browse, Confirm, Field, Hits, Hover, Panes, Screen, Start, View};
use crate::{fmt_size, Scan};

/// The flags that keep an item from the trash, shown with a lock.
const LOCKED: u16 = PROTECTED | NOT_OWNED | MOUNT | UNREADABLE;

/// The colours and glyphs of one frame, and the pointer where it may show hover (`None` where
/// nothing under it takes a click: under the dialog, the help).
#[derive(Clone, Copy)]
struct Look {
    c: Colors,
    g: &'static Glyphs,
    pointer: Option<(u16, u16)>,
}

impl Look {
    fn on(&self, bg: Color) -> Style {
        Style::new().bg(bg).fg(self.c.fg)
    }

    /// The pointer is over `r`.
    fn over(&self, r: Rect) -> bool {
        self.pointer
            .is_some_and(|(x, y)| x >= r.x && x < r.right() && y >= r.y && y < r.bottom())
    }
}

/// The treemap's classes: by type (from the name only) and by age, each with a letter that
/// shows it without colour, on the tiles and in the legend.
const KINDS: [(&str, &str); 8] = [
    ("/", "folder"),
    ("V", "video"),
    ("I", "image"),
    ("A", "audio"),
    ("Z", "archive"),
    ("D", "doc"),
    ("T", "text"),
    ("O", "other"),
];
const AGES: [(&str, &str); 4] = [
    ("W", "this week"),
    ("M", "this month"),
    ("Y", "this year"),
    ("O", "older"),
];

/// The class (`KINDS`) of an item, from its name's extension only.
pub(crate) fn kind(name: &[u8], dir: bool) -> usize {
    if dir {
        return 0;
    }
    let ext = match name.iter().rposition(|&b| b == b'.') {
        Some(i) if i > 0 && name.len() - i <= 9 => &name[i + 1..],
        _ => return 7,
    };
    let mut low = [0u8; 8];
    let low = &mut low[..ext.len()];
    low.copy_from_slice(ext);
    low.make_ascii_lowercase();
    match &*low {
        b"mp4" | b"mkv" | b"webm" | b"avi" | b"mov" | b"m4v" | b"wmv" | b"flv" | b"mpg"
        | b"mpeg" | b"3gp" | b"ogv" => 1,
        b"png" | b"jpg" | b"jpeg" | b"gif" | b"webp" | b"bmp" | b"svg" | b"tif" | b"tiff"
        | b"ico" | b"heic" | b"avif" | b"raw" | b"cr2" | b"nef" | b"psd" | b"xcf" => 2,
        b"mp3" | b"flac" | b"ogg" | b"opus" | b"wav" | b"m4a" | b"aac" | b"wma" | b"mid" => 3,
        b"zip" | b"tar" | b"gz" | b"tgz" | b"bz2" | b"xz" | b"zst" | b"7z" | b"rar" | b"iso"
        | b"deb" | b"rpm" | b"snap" | b"appimage" | b"jar" | b"whl" | b"lz4" | b"img" => 4,
        b"pdf" | b"doc" | b"docx" | b"odt" | b"rtf" | b"xls" | b"xlsx" | b"ods" | b"ppt"
        | b"pptx" | b"odp" | b"epub" | b"djvu" => 5,
        b"txt" | b"md" | b"rst" | b"json" | b"yaml" | b"yml" | b"toml" | b"xml" | b"html"
        | b"htm" | b"css" | b"js" | b"ts" | b"tsx" | b"jsx" | b"py" | b"rs" | b"c" | b"h"
        | b"cpp" | b"hpp" | b"cc" | b"go" | b"java" | b"kt" | b"rb" | b"php" | b"sh" | b"lua"
        | b"sql" | b"log" | b"csv" | b"ini" | b"conf" | b"cfg" => 6,
        _ => 7,
    }
}

/// The class (`AGES`) of a modification time, `now` being the time of the drawing.
fn age(mtime: i64, now: i64) -> usize {
    const DAY: i64 = 86_400;
    match now - mtime {
        d if d < 7 * DAY => 0,
        d if d < 30 * DAY => 1,
        d if d < 365 * DAY => 2,
        _ => 3,
    }
}

/// Draws the view over `area` of `buf`, and nothing outside it. Everything goes through `put`,
/// `frame` and `Buffer::set_style`, which clip to the buffer: a view that only gets part of the
/// buffer is drawn into its own canvas the size of its area, then copied in.
pub(crate) fn draw(v: &mut View, area: Rect, buf: &mut Buffer) {
    let area = area.intersection(buf.area);
    if area == buf.area {
        return paint(v, area, buf);
    }
    let mut canvas = std::mem::replace(&mut v.canvas, Buffer::empty(Rect::default()));
    if canvas.area != area {
        canvas = Buffer::empty(area);
    }
    paint(v, area, &mut canvas);
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            buf[(x, y)] = canvas[(x, y)].clone();
        }
    }
    v.canvas = canvas;
}

fn paint(v: &mut View, area: Rect, buf: &mut Buffer) {
    let look = Look {
        c: v.colors,
        g: v.icons.glyphs(),
        pointer: v.pointer.filter(|_| !v.help),
    };
    let (c, g) = (&look.c, look.g);
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            let cell = &mut buf[(x, y)];
            cell.reset();
            (cell.bg, cell.fg) = (c.bg, c.fg);
        }
    }
    let mut hits = Hits::default();
    if area.width < 24 || area.height < 6 {
        put(
            buf,
            area.x,
            area.y,
            "termdisk: too small",
            area.width,
            look.on(c.bg),
        );
        if let Screen::Browse(b) = &mut v.screen {
            if let Some(c) = &mut b.confirm {
                c.fits = false; // nothing of it is shown, so nothing can be moved
            }
        }
        v.hits = hits;
        v.hover = Hover::Nothing;
        return;
    }
    let body = Rect::new(area.x, area.y + 1, area.width, area.height - 2);
    let (status, mut hints) = (area.bottom() - 1, String::new());
    let [up_down, right_left, back] = g.keys;
    let sep = g.sep;
    // The status bar's items (wide enough only): they take the place of some hints.
    let items_fit = area.width >= 76;
    let title = match &mut v.screen {
        Screen::Start(s) => {
            hints = format!("{up_down} choose  Enter scan  ? help  q quit");
            start(s, &v.policy, body, buf, &mut hits, &look);
            format!("choose what to look at  {sep}  termdisk")
        }
        Screen::Scanning(s) => {
            hints.push_str("Esc stop  q quit");
            scanning(s, v.scan_started.elapsed(), body, buf, &look)
        }
        Screen::Browse(b) => {
            let read_only = v.policy.read_only().is_some();
            hints = format!("{up_down} move  {right_left} open  Enter zoom  {back} up  ");
            if !read_only {
                hints.push_str("Del trash  ");
            }
            match items_fit {
                true => hints.push_str("v panes  r rescan  q quit"),
                false => hints.push_str(
                    "o open  c copy  a size  t letters  v panes  r rescan  ? help  q quit",
                ),
            }
            // Under the dialog nothing takes a click, so nothing there shows hover.
            let panes = Look {
                pointer: look.pointer.filter(|_| b.confirm.is_none()),
                ..look
            };
            let modes = (v.apparent, v.by_age);
            browse(b, modes, v.panes, body, buf, &mut hits, &panes);
            let zoom = b.tree.node(b.zoom);
            let size = if v.apparent {
                zoom.apparent
            } else {
                zoom.alloc
            };
            let kind = if v.apparent {
                "apparent sizes"
            } else {
                "disk usage"
            };
            let only = if b.trash_reaches || read_only {
                String::new()
            } else {
                format!("  {sep}  look only: on another filesystem than the trash")
            };
            let path = shown(b.tree.path(b.zoom).as_os_str());
            format!(
                "{path}  {sep}  {} ({kind}){only}  {sep}  termdisk",
                fmt_size(size)
            )
        }
        Screen::Gone => String::new(),
    };
    // termcode's title bar: the text in the middle.
    let st = Style::new().bg(c.title_bg).fg(c.title_fg);
    buf.set_style(Rect::new(area.x, area.y, area.width, 1), st);
    // The close button takes the last 3 cells; the title keeps clear of it on both sides.
    let t = fit_end(&title, area.width.saturating_sub(8) as usize, g.cut);
    let x = area.x + (area.width - width(&t) as u16) / 2;
    put(buf, x, area.y, &t, area.width, st);
    let close = Rect::new(area.right() - 3, area.y, 3, 1);
    let cross = match look.over(close) && v.close_works() {
        true => Style::new().bg(c.close_hover).fg(c.button_fg),
        false => st.fg(c.close_fg),
    };
    put(
        buf,
        close.x,
        close.y,
        &format!(" {} ", g.close),
        3,
        cross.add_modifier(Modifier::BOLD),
    );
    hits.close = Some(close);
    let dot = g.dot;
    let line = match (&v.message, v.policy.read_only()) {
        (Some(m), _) if v.done => format!(" {} {}", g.check, clean(m)),
        (Some(m), _) => format!(" {}", clean(m)),
        (None, Some(why)) => format!(" {why}  {dot}  {hints}"),
        (None, None) => format!(" {hints}"),
    };
    let st = Style::new().bg(c.status_bg).fg(c.status_fg);
    buf.set_style(Rect::new(area.x, status, area.width, 1), st);
    // termcode's status bar items, at the right: each does what its key does.
    let items: Vec<(String, char)> = match &v.screen {
        Screen::Browse(b) if b.confirm.is_none() && items_fit => {
            let size = if v.apparent {
                "apparent sizes"
            } else {
                "disk usage"
            };
            let map = if v.by_age { "by age" } else { "by type" };
            vec![
                (format!("a: {size}"), 'a'),
                (format!("t: {map}"), 't'),
                ("?: keys".into(), '?'),
            ]
        }
        _ => Vec::new(),
    };
    let mut x = area.right() - items.iter().map(|i| width(&i.0) as u16 + 2).sum::<u16>();
    let left = x - area.x;
    for (text, k) in items {
        let r = Rect::new(x, status, width(&text) as u16 + 2, 1);
        let s = if look.over(r) {
            st.bg(c.status_hover)
        } else {
            st
        };
        put(buf, x, status, &format!(" {text} "), r.width, s);
        hits.items.push((r, k));
        x += r.width;
    }
    let line = fit_end(&line, left as usize, g.cut);
    put(buf, area.x, status, &line, left, st);
    if let Screen::Browse(b) = &mut v.screen {
        let Browse { tree, confirm, .. } = &mut **b;
        if let Some(c) = confirm {
            dialog(c, tree, v.apparent, area, buf, &mut hits, &look);
        }
    }
    if v.help {
        help(area, buf, &look);
    }
    v.hits = hits;
    // What the pointer is over in this drawing: a move only redraws when that changes.
    v.hover = v.pointer.map_or(Hover::Nothing, |(x, y)| v.hover_at(x, y));
}

fn start(s: &Start, policy: &Policy, area: Rect, buf: &mut Buffer, hits: &mut Hits, look: &Look) {
    let (c, g) = (&look.c, look.g);
    let x = area.x + 2;
    let w = area.width.saturating_sub(4);
    let bold = look.on(c.bg).add_modifier(Modifier::BOLD);
    put(buf, x, area.y + 1, "Places", w, bold);
    let list = Rect::new(area.x, area.y + 2, area.width, s.places.len() as u16);
    hits.places = list;
    for (i, p) in s.places.iter().enumerate() {
        let y = list.y + i as u16;
        let selected = i == s.sel;
        let style = if selected {
            Style::new()
                .bg(c.sel_bg)
                .fg(c.sel_fg)
                .add_modifier(Modifier::BOLD)
        } else if look.over(Rect::new(list.x, y, list.width, 1)) {
            look.on(c.hover_bg)
        } else {
            look.on(c.bg)
        };
        let note = if !p.exists {
            "not there"
        } else if !p.trashable && policy.read_only().is_none() {
            "look only: on another filesystem than the trash"
        } else {
            ""
        };
        let path = clean(&p.path.to_string_lossy());
        let icon = icon_gap(g.folder);
        let text = format!(" {icon}{:<16} {path}", p.name);
        buf.set_style(Rect::new(x, y, w, 1), style);
        if selected {
            put(buf, x - 1, y, g.mark, 1, look.on(c.bg).fg(c.focus));
        }
        put(buf, x, y, &fit_end(&text, w as usize, g.cut), w, style);
        let nw = note.chars().count() as u16;
        if !note.is_empty() && w > nw + 40 {
            let st = if selected { style } else { style.fg(c.dim) };
            put(buf, x + w - nw - 1, y, note, nw, st);
        }
    }
    if let Some((used, total)) = s.disk {
        let y = list.bottom() + 1;
        let share = used as f64 / total.max(1) as f64;
        let text = format!(
            "Home filesystem  {} used of {}  ",
            fmt_size(used),
            fmt_size(total)
        );
        put(buf, x, y, &text, w, look.on(c.bg));
        let bw = w.saturating_sub(width(&text) as u16).min(30);
        draw_bar(
            buf,
            x + width(&text) as u16,
            y,
            share,
            bw,
            look.on(c.bg),
            look,
        );
    }
}

/// A running scan: a card with a spinner, the counts, the rate and the folder being read. It
/// moves only when drawn, and a scan is drawn every 100 ms (`View::tick`), so nothing wakes
/// for it.
fn scanning(s: &Scan, since: Duration, area: Rect, buf: &mut Buffer, look: &Look) -> String {
    use std::sync::atomic::Ordering::Relaxed;
    let (c, g) = (&look.c, look.g);
    let p = s.progress();
    let dot = g.dot;
    let (files, dirs) = (p.files.load(Relaxed), p.dirs.load(Relaxed));
    let spin = g.spin[(since.as_millis() / 100) as usize % g.spin.len()];
    let secs = since.as_secs_f64();
    let rate = (files + dirs) as f64 / secs.max(0.1);
    let lines = [
        format!(
            "{spin} {files} files  {dot}  {dirs} folders  {dot}  {}",
            fmt_size(p.bytes.load(Relaxed))
        ),
        format!("{rate:.0} items/s  {dot}  {secs:.1} s"),
        match p.errors.load(Relaxed) {
            0 => String::new(),
            n => format!("{n} could not be read"),
        },
    ];
    // On a card in the middle where there is room, else at the top left.
    let card = Rect::new(
        area.x + area.width.saturating_sub(72) / 2,
        area.y + area.height.saturating_sub(8) / 2,
        area.width.min(72),
        8,
    );
    let boxed = area.width >= 40 && area.height >= 8;
    let (x, mut y, w) = match boxed {
        true => (card.x + 2, card.y + 2, card.width - 4),
        false => (area.x + 2, area.y + 1, area.width.saturating_sub(4)),
    };
    let style = match boxed {
        true => Style::new().bg(c.widget_bg).fg(c.widget_fg),
        false => look.on(c.bg),
    };
    if boxed {
        frame(card, " Scanning ", style, look, buf);
    }
    put(buf, x, y, &lines[0], w, style.add_modifier(Modifier::BOLD));
    for line in &lines[1..] {
        y += 1;
        put(buf, x, y, line, w, style.fg(c.dim));
    }
    let current = p.current.lock().map(|c| c.clone()).unwrap_or_default();
    let current = clean(&current.to_string_lossy());
    let current = fit_middle(&current, w as usize, g.cut);
    put(buf, x, y + 1, &current, w, style.fg(c.dim));
    format!("scanning  {}  termdisk", g.sep)
}

/// `modes`: apparent sizes, the treemap by age.
fn browse(
    b: &mut Browse,
    (apparent, by_age): (bool, bool),
    panes: Panes,
    area: Rect,
    buf: &mut Buffer,
    hits: &mut Hits,
    look: &Look,
) {
    let (tree_area, map_area) = match panes {
        Panes::Tree => (Some(area), None),
        Panes::Map => (None, Some(area)),
        Panes::Both => {
            let left = (area.width / 2).max(40).min(area.width.saturating_sub(20));
            let left = if area.width < 60 { area.width } else { left };
            let map = Rect::new(
                area.x + left + 1,
                area.y,
                area.width.saturating_sub(left + 1),
                area.height,
            );
            let map = (map.width >= 10).then_some(map);
            (Some(Rect::new(area.x, area.y, left, area.height)), map)
        }
    };
    if let Some(r) = tree_area {
        tree_pane(b, apparent, r, buf, hits, look);
    }
    if let Some(r) = map_area {
        map_pane(b, (apparent, by_age), r, buf, hits, look);
    }
}

fn tree_pane(
    b: &mut Browse,
    apparent: bool,
    r: Rect,
    buf: &mut Buffer,
    hits: &mut Hits,
    look: &Look,
) {
    let (c, g) = (&look.c, look.g);
    if r.width < 2 {
        return;
    }
    buf.set_style(r, look.on(c.side_bg));
    let head = Style::new()
        .bg(c.header_bg)
        .fg(c.header_fg)
        .add_modifier(Modifier::BOLD);
    buf.set_style(Rect::new(r.x, r.y, r.width, 1), head);
    // The last column is the scrollbar's, shown or not, so nothing moves when it comes; rows
    // (and their clicks) end before it.
    let bar_x = r.right() - 1;
    let r = Rect::new(r.x, r.y, r.width - 1, r.height);
    // A click on a heading sorts by it (Usage by size); the sorted one has an arrow.
    let sort = b.sort;
    let field = |title: &str| match title {
        "Name" => Field::Name,
        "Items" => Field::Items,
        "Modified" => Field::Modified,
        _ => Field::Size,
    };
    let sorted = |title: &str| title != "Usage" && field(title) == sort.by;
    // In a narrow pane columns go, Items first, so names keep 24 cells; the sorted one stays
    // while another can go, and goes too where it alone has no room. Usage is a percentage, a
    // space and a bar of 8.
    let mut cols: Vec<(&str, u16)> =
        vec![("Usage", 13), ("Size", 9), ("Items", 7), ("Modified", 10)];
    let fits = |cols: &[(&str, u16)]| r.width >= 24 + cols.iter().map(|c| c.1 + 1).sum::<u16>();
    for drop in ["Items", "Modified", "Usage", "Size"] {
        if fits(&cols) {
            break;
        }
        if !sorted(drop) {
            cols.retain(|c| c.0 != drop);
        }
    }
    if !fits(&cols) {
        cols.clear();
    }
    let right: u16 = cols.iter().map(|c| c.1 + 1).sum();
    let name_w = r.width.saturating_sub(right);
    let mut heading = |title: &str, at: Rect, right_aligned: bool| {
        let arrow = match sorted(title) {
            true => g.sort[usize::from(sort.descending)],
            false => "",
        };
        // Hover underlines the heading's text only, the same for every column.
        let t = format!("{title}{arrow}");
        let x = match right_aligned {
            true => at.x + at.width.saturating_sub(width(&t) as u16),
            false => at.x,
        };
        let st = match look.over(at) {
            true => head.add_modifier(Modifier::UNDERLINED),
            false => head,
        };
        put(buf, x, at.y, &t, at.right() - x, st);
        // A faint line in the space before each column after the name.
        if right_aligned {
            let line = Style::new().bg(c.header_bg).fg(c.column_line);
            put(buf, at.x - 1, at.y, g.border[5], 1, line);
        }
        hits.heads.push((at, field(title)));
    };
    let name_at = Rect::new(r.x + 1, r.y, name_w.saturating_sub(1), 1);
    heading("Name", name_at, false);
    let mut x = r.x + name_w;
    for (title, w) in &cols {
        heading(title, Rect::new(x + 1, r.y, *w, 1), true);
        x += w + 1;
    }
    let rows = Rect::new(r.x, r.y + 1, r.width, r.height.saturating_sub(1));
    hits.rows = rows;
    hits.scrollbar = Rect::new(bar_x, rows.y, 1, rows.height);
    let h = rows.height as usize;
    b.page = h.max(1);
    if b.cursor < b.top {
        b.top = b.cursor;
    } else if b.cursor >= b.top + h {
        b.top = b.cursor + 1 - h;
    }
    b.top = b.top.min(b.rows.len().saturating_sub(1));
    for (i, &(id, depth)) in b.rows.iter().enumerate().skip(b.top).take(h) {
        let y = rows.y + (i - b.top) as u16;
        let n = b.tree.node(id);
        let selected = i == b.cursor;
        let locked = n.flags & LOCKED != 0;
        // The selection: its colours, bold, and a mark at the left edge (no colour needed).
        let bg = match look.over(Rect::new(r.x, y, r.width, 1)) {
            true => c.hover_bg,
            false => c.side_bg,
        };
        let style = if selected {
            Style::new()
                .bg(c.sel_bg)
                .fg(c.sel_fg)
                .add_modifier(Modifier::BOLD)
        } else if locked {
            Style::new().bg(bg).fg(c.dim)
        } else {
            look.on(bg)
        };
        buf.set_style(Rect::new(r.x, y, r.width, 1), style);
        if selected {
            put(buf, r.x, y, g.mark, 1, style.fg(c.focus));
        }
        // termcode's row: [indent][chevron][space][icon][space][name]; each level of the indent
        // is a guide and a space.
        let open = b.open.contains(&id) || id == b.zoom;
        let (chev, icon) = match (n.is_dir(), open) {
            (true, true) => (g.open, g.folder_open),
            (true, false) => (g.closed, g.folder),
            _ => (" ", g.file),
        };
        let levels = depth.min(12);
        let indent = format!("{} ", g.guide).repeat(levels as usize);
        let lead = format!("{indent}{chev} {}", icon_gap(icon));
        let mut name = name_of(b, id);
        if n.is_dir() && id != 0 {
            name.push('/');
        }
        let lock = if locked {
            format!(" {}", g.lock)
        } else {
            String::new()
        };
        let link = if n.flags & (LINKED | HARDLINK_DUP) != 0 {
            " (hard link)"
        } else {
            ""
        };
        let full = format!("{lead}{name}{lock}{link}");
        let room = name_w.saturating_sub(2) as usize;
        let shown = fit_end(&full, room, g.cut);
        put(buf, r.x + 1, y, &shown, name_w, style);
        // Each level's colour on its guide, and on the chevron and folder icon of a row at that
        // level (the selected row keeps its own); only as far as the text is shown, not over
        // where it is cut.
        if !selected {
            let cut = usize::from(width(&full) > room);
            let end = r.x + 1 + width(&shown).saturating_sub(cut) as u16;
            let mut paint = |x: u16, glyph: &str, fg: Color| {
                if x < end {
                    put(buf, x, y, glyph, 1, style.fg(fg));
                }
            };
            let level = |d: u16| c.depth[usize::from(d) % c.depth.len()];
            for d in 0..levels {
                paint(r.x + 1 + 2 * d, g.guide, level(d));
            }
            let x = r.x + 1 + width(&indent) as u16;
            let fg = if n.is_dir() { level(depth) } else { c.icon_fg };
            paint(x, chev, fg);
            if !icon.is_empty() {
                paint(x + 2, icon, fg);
            }
        }
        if locked && width(&full) <= room {
            let x = r.x + 1 + width(&lead) as u16 + width(&name) as u16 + 1;
            put(buf, x, y, g.lock, 1, style.fg(c.lock));
        }
        let parent = b
            .tree
            .parent(id)
            .filter(|_| id != b.zoom)
            .map(|p| *b.tree.node(p));
        let mut x = r.x + name_w;
        let quiet = if selected { style } else { style.fg(c.dim) };
        for (title, w) in &cols {
            put(buf, x, y, g.border[5], 1, style.fg(c.column_line));
            match *title {
                "Usage" => {
                    let total = parent.map_or(counted(n, apparent), |p| counted(&p, apparent));
                    let share = counted(n, apparent) as f64 / total.max(1) as f64;
                    put(buf, x + 1, y, &format!("{:>4}", percent(share)), 4, style);
                    draw_bar(buf, x + 6, y, share, w - 5, style, look);
                }
                "Size" => {
                    let s = fmt_size(if apparent { n.apparent } else { n.alloc });
                    put(
                        buf,
                        x + 1,
                        y,
                        &format!("{s:>w$}", w = *w as usize),
                        *w,
                        style,
                    );
                }
                "Items" if n.is_dir() => {
                    let t = format!("{:>w$}", n.items, w = *w as usize);
                    put(buf, x + 1, y, &t, *w, quiet);
                }
                "Modified" => {
                    let t = &sys::local_datetime(n.mtime)[..10];
                    put(buf, x + 1, y, t, *w, quiet);
                }
                _ => {}
            }
            x += w + 1;
        }
    }
    if b.rows.len() > h && h > 0 {
        let more = b.rows.len() - h;
        let (start, len) = slider(h as u16, b.rows.len(), h, b.top, more);
        let st = look.on(c.side_bg).fg(c.slider);
        for y in rows.y + start..rows.y + start + len {
            put(buf, bar_x, y, g.slider, 1, st);
        }
    }
}

/// A scrollbar's slider (first row, rows) on a `track` of rows, for a view of `view` rows at
/// `off` (0..=max_off) over `total` rows. From termcode's `decor::slider` (MIT, the same
/// author).
fn slider(track: u16, total: usize, view: usize, off: usize, max_off: usize) -> (u16, u16) {
    if track == 0 {
        return (0, 0);
    }
    if max_off == 0 || total == 0 {
        return (0, track);
    }
    let len = ((track as usize * view) as f64 / total as f64)
        .round()
        .clamp(1.0, track as f64) as u16;
    let free = (track - len) as usize;
    let start = ((free * off.min(max_off)) as f64 / max_off as f64).round() as u16;
    (start.min(track - len), len)
}

/// The treemap of the folder that holds the selected row (of the shown folder on its own row),
/// largest first, under a legend: that folder's name and the classes on the map. The selected
/// item's tile is blue, every other one in the next colour of `Colors::tiles` (`modes`: apparent
/// sizes, the letters by age). Every tile shows its class as a letter too and has a frame where
/// it has room (white on the selected one, which also has the mark in its corner): all inside
/// the tile, so a click on them picks that tile.
fn map_pane(
    b: &mut Browse,
    (apparent, by_age): (bool, bool),
    r: Rect,
    buf: &mut Buffer,
    hits: &mut Hits,
    look: &Look,
) {
    let (c, g) = (&look.c, look.g);
    let (folder, selected) = b.map_of();
    let kids = b.kids(folder).to_vec();
    let sizes: Vec<u64> = kids
        .iter()
        .map(|&k| counted(b.tree.node(k), apparent))
        .collect();
    let selected = selected.and_then(|s| kids.iter().position(|&k| k == s));
    // The "+N items" tile holds the N smallest items that are not empty (the last ones).
    let not_empty = sizes.iter().filter(|&&s| s > 0).count();
    let (legend, r) = match r.height >= 3 {
        true => (Some(r.y), Rect::new(r.x, r.y + 1, r.width, r.height - 1)),
        false => (None, r),
    };
    let classes: &[(&str, &str)] = match by_age {
        true => &AGES,
        false => &KINDS,
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let mut shown = [false; 8];
    for t in treemap::layout(&sizes, r) {
        let rect = t.rect;
        // A gap at the right and bottom keeps tiles of one colour apart.
        let fill = Rect::new(
            rect.x,
            rect.y,
            rect.width - u16::from(rect.width > 3),
            rect.height - u16::from(rect.height > 2),
        );
        let (id, label, detail, class) = match t.item {
            Item::One(k) => {
                let id = kids[k];
                let n = b.tree.node(id);
                let class = match by_age {
                    true => age(n.mtime, now),
                    false => kind(b.tree.name(id).as_bytes(), n.is_dir()),
                };
                shown[class] = true;
                let mut name = name_of(b, id);
                if n.is_dir() {
                    name.push('/');
                }
                if n.flags & LOCKED != 0 {
                    name.push(' ');
                    name.push_str(g.lock);
                }
                (Some(id), name, fmt_size(sizes[k]), Some(class))
            }
            Item::Rest(count) => (None, format!("+{count} items"), String::new(), None),
        };
        let chosen = selected.is_some_and(|s| match t.item {
            Item::One(k) => k == s,
            Item::Rest(count) => s < not_empty && s + count >= not_empty,
        });
        let style = match (chosen, t.item) {
            (true, _) => Style::new()
                .bg(c.sel_tile)
                .fg(c.tile_fg)
                .add_modifier(Modifier::BOLD),
            (false, Item::One(k)) => Style::new().bg(c.tiles[k % c.tiles.len()]).fg(c.tile_fg),
            (false, Item::Rest(_)) => Style::new().bg(c.rest_bg).fg(c.rest_fg),
        };
        buf.set_style(fill, style);
        // Every tile has a frame where it has room: dark on the others, white on the selected.
        let outlined = fill.width >= 4 && fill.height >= 3;
        if outlined {
            let edge = if chosen { style } else { style.fg(c.bg) };
            outline(fill, edge, look, buf);
        }
        let w = fill.width.saturating_sub(1 + u16::from(outlined));
        let top = match look.over(rect) && !chosen {
            true => style.add_modifier(Modifier::UNDERLINED),
            false => style,
        };
        // The class letter first, kept however small the tile.
        let label = match class {
            Some(k) if w >= 3 => format!(
                "{} {}",
                classes[k].0,
                fit_end(&label, w as usize - 2, g.cut)
            ),
            Some(k) => classes[k].0.to_string(),
            None => fit_end(&label, w as usize, g.cut),
        };
        put(buf, fill.x + 1, fill.y, &label, w, top);
        if fill.height > 1 {
            put(buf, fill.x + 1, fill.y + 1, &detail, w, style);
        }
        if chosen {
            put(buf, fill.x, fill.y, g.mark, 1, style); // the selected tile, without colour
        }
        hits.tiles.push((rect, id));
    }
    // The legend: the folder on the map (a third of the line at most), then the classes on it,
    // each by its letter; where a name has no room, the letter alone (the keys, `?`, list them
    // all).
    let Some(y) = legend else { return };
    let mut name = name_of(b, folder);
    if folder != 0 {
        name.push('/');
    }
    let name = fit_middle(&name, (r.width / 3) as usize, g.cut);
    let bold = look.on(c.bg).add_modifier(Modifier::BOLD);
    let label = format!("{name} {}", g.dot);
    put(buf, r.x + 1, y, &label, r.width, bold);
    let mut x = r.x + 1 + width(&label) as u16 + 1;
    for (k, &(letter, name)) in classes.iter().enumerate() {
        if !shown[k] {
            continue;
        }
        let full = 2 + width(name) as u16;
        let w = if x + full <= r.right() { full } else { 1 };
        if x + w > r.right() {
            break;
        }
        put(buf, x, y, letter, 1, bold);
        if w > 1 {
            put(buf, x + 2, y, name, w - 2, look.on(c.bg).fg(c.dim));
        }
        x += w + 1;
    }
}

/// A box's edges, in the set's border glyphs, inside `r`: the cells it covers keep their place.
fn outline(r: Rect, style: Style, look: &Look, buf: &mut Buffer) {
    if r.width < 2 || r.height < 2 {
        return;
    }
    let [tl, tr, bl, br, across, down] = look.g.border;
    let (right, bottom) = (r.right() - 1, r.bottom() - 1);
    for x in r.x + 1..right {
        put(buf, x, r.y, across, 1, style);
        put(buf, x, bottom, across, 1, style);
    }
    for y in r.y + 1..bottom {
        put(buf, r.x, y, down, 1, style);
        put(buf, right, y, down, 1, style);
    }
    for (x, y, s) in [
        (r.x, r.y, tl),
        (right, r.y, tr),
        (r.x, bottom, bl),
        (right, bottom, br),
    ] {
        put(buf, x, y, s, 1, style);
    }
}

/// The trash dialog. It shows the whole path, wrapped and escaped (`shown`); if the window
/// cannot hold it all, it says so, and the item cannot be moved (`Confirm::fits`).
fn dialog(
    c: &mut Confirm,
    tree: &Tree,
    apparent: bool,
    area: Rect,
    buf: &mut Buffer,
    hits: &mut Hits,
    look: &Look,
) {
    let (k, g) = (&look.c, look.g);
    let n = tree.node(c.id);
    let linked = n.flags & (LINKED | HARDLINK_DUP) != 0;
    let w = area.width.saturating_sub(2).min(80);
    let iw = w.saturating_sub(4);
    let lines = wrap(&shown(c.path.as_os_str()), iw as usize);
    // Border, blank, the path, the size, (hard links), the trash line, blank, buttons, border.
    let full = 7 + lines.len() as u16 + u16::from(linked);
    c.fits = iw >= 33 && full <= area.height;
    let h = if c.fits { full } else { 7 };
    let r = Rect::new(
        area.x + (area.width - w) / 2,
        area.y + area.height.saturating_sub(h) / 2,
        w,
        h,
    );
    let style = Style::new().bg(k.widget_bg).fg(k.widget_fg);
    frame(r, " Move to the trash? ", style, look, buf);
    let x = r.x + 2;
    let mut y = r.y + 2;
    if c.fits {
        let bold = style.add_modifier(Modifier::BOLD);
        for line in &lines {
            put(buf, x, y, line, iw, bold);
            y += 1;
        }
        let size = fmt_size(if apparent { n.apparent } else { n.alloc });
        let what = if n.is_dir() {
            format!("{size}  {}  {} items", g.dot, n.items)
        } else {
            size
        };
        put(buf, x, y, &what, iw, style);
        y += 1;
        if linked {
            let note = "It has other hard links: moving it may free nothing.";
            put(buf, x, y, note, iw, style);
            y += 1;
        }
        let note = "The space is freed only when the trash is emptied.";
        put(buf, x, y, note, iw, style);
    } else {
        let note = "The window is too small to show the whole path:";
        put(buf, x, y, note, iw, style);
        y += 1;
        put(buf, x, y, "make it larger, or press Esc.", iw, style);
    }
    y += 2;
    // termcode's buttons: the one with the focus primary (and bold), the other secondary. A
    // Move to Trash that cannot be used yet (or here: the path does not fit) is dim and
    // crossed out as well as grey.
    let primary = Style::new()
        .bg(k.button_bg)
        .fg(k.button_fg)
        .add_modifier(Modifier::BOLD);
    let secondary = Style::new().bg(k.button2_bg).fg(k.button2_fg);
    let cancel = Rect::new(x + 2, y, 10, 1);
    let moving = Rect::new(x + 16, y, 17, 1);
    // Hover lightens a button that takes a click; a disabled one stays as it is.
    let lit = |r: Rect, s: Style, main: bool| match (look.over(r), main) {
        (false, _) => s,
        (true, true) => s.bg(k.button_hover),
        (true, false) => s.bg(k.button2_hover),
    };
    let cancel_style = match c.on_move {
        true => lit(cancel, secondary, false),
        false => lit(cancel, primary, true),
    };
    put(
        buf,
        cancel.x,
        cancel.y,
        "[ Cancel ]",
        cancel.width,
        cancel_style,
    );
    // Disabled exactly when a click would not move it (`clickable`): the same rule, no other.
    let move_style = if !c.clickable() {
        let off = Style::new()
            .bg(k.button2_bg)
            .fg(k.disabled)
            .add_modifier(Modifier::CROSSED_OUT | Modifier::DIM);
        if c.on_move {
            off.add_modifier(Modifier::BOLD)
        } else {
            off
        }
    } else if c.on_move {
        lit(moving, primary, true)
    } else {
        lit(moving, secondary, false)
    };
    put(
        buf,
        moving.x,
        moving.y,
        "[ Move to Trash ]",
        moving.width,
        move_style,
    );
    hits.buttons = Some((cancel, moving));
}

fn help(area: Rect, buf: &mut Buffer, look: &Look) {
    let [up_down, right_left, _] = look.g.keys;
    let (moves, opens) = (
        format!("{up_down}  PgUp PgDn"),
        format!("{right_left}  + -"),
    );
    let keys: [(&str, &str); 20] = [
        (&moves, "move"),
        (&opens, "open, close a folder"),
        ("double click (tree)", "open, close a folder"),
        ("Enter, double click (map)", "show that folder"),
        ("Backspace", "show the folder above"),
        ("Del", "move to the trash (asks first)"),
        ("o", "open with the default program"),
        ("c", "copy the path"),
        ("a", "disk usage / apparent size"),
        ("t", "map letters: by type / by age"),
        ("s, S", "sort the tree: next column / reverse"),
        ("v", "tree and map / tree / map"),
        ("r", "scan again"),
        ("Esc", "stop a scan, close a dialog"),
        ("click", "select (tree or map), sort (heading)"),
        ("?", "this help"),
        ("q", "quit"),
        // The map's letters, all of them (its legend shows those on it, as room allows).
        ("map: / V I A Z", "folder video image audio archive"),
        ("     D T O", "document, code and text, other"),
        ("by age: W M Y O", "this week, month, year, older"),
    ];
    let w = area.width.saturating_sub(4).min(64);
    let h = (keys.len() as u16 + 4).min(area.height);
    let r = Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    );
    let style = Style::new().bg(look.c.widget_bg).fg(look.c.widget_fg);
    frame(r, " Keys ", style, look, buf);
    for (i, (k, what)) in keys.iter().enumerate().take(h.saturating_sub(4) as usize) {
        let line = format!("{k:<27}{what}");
        put(
            buf,
            r.x + 2,
            r.y + 2 + i as u16,
            &line,
            w.saturating_sub(4),
            style,
        );
    }
}

/// termcode's popup: a box with rounded corners (ASCII ones in the ASCII set) in the widget's
/// border colour, filled with `style`, a title on its top edge. Only the part inside the
/// buffer is drawn.
fn frame(r: Rect, title: &str, style: Style, look: &Look, buf: &mut Buffer) {
    let [tl, tr, bl, br, across, down] = look.g.border;
    buf.set_style(r, style);
    let seen = r.intersection(buf.area);
    for y in seen.y..seen.bottom() {
        for x in seen.x..seen.right() {
            let edge_x = x == r.x || x + 1 == r.right();
            let edge_y = y == r.y || y + 1 == r.bottom();
            let s = match (edge_x, edge_y, x == r.x, y == r.y) {
                (true, true, true, true) => tl,
                (true, true, false, true) => tr,
                (true, true, true, false) => bl,
                (true, true, false, false) => br,
                (false, true, _, _) => across,
                (true, false, _, _) => down,
                _ => " ",
            };
            let cell = &mut buf[(x, y)];
            cell.set_symbol(s);
            if edge_x || edge_y {
                cell.set_fg(look.c.widget_border);
            }
        }
    }
    put(
        buf,
        r.x + 2,
        r.y,
        title,
        r.width.saturating_sub(4),
        style.add_modifier(Modifier::BOLD),
    );
}

/// A node's name as shown (`shown`): the root's is its whole path.
fn name_of(b: &Browse, id: u32) -> String {
    shown(b.tree.name(id))
}

/// A name or path made safe to show, and still exact: control characters as escapes (`\n`,
/// `\x1b`), bytes that are not UTF-8 as `\xff`, a backslash as `\\`, and the characters that
/// reorder text (bidi controls) as `\u{202e}`, so nothing can hide or rearrange what is shown.
pub(crate) fn shown(s: &OsStr) -> String {
    let mut out = String::new();
    for chunk in s.as_bytes().utf8_chunks() {
        for c in chunk.valid().chars() {
            match c {
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\t' => out.push_str("\\t"),
                '\r' => out.push_str("\\r"),
                c if c.is_control() => out.push_str(&format!("\\x{:02x}", c as u32)),
                c if bidi(c) => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
                c => out.push(c),
            }
        }
        for b in chunk.invalid() {
            out.push_str(&format!("\\x{b:02x}"));
        }
    }
    out
}

/// The characters that change the direction text is shown in.
fn bidi(c: char) -> bool {
    matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// A message as shown: control and bidi characters as `?` (paths in it come from the guard's
/// own text).
fn clean(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() || bidi(c) { '?' } else { c })
        .collect()
}

/// `s` in lines of at most `w` cells.
pub(crate) fn wrap(s: &str, w: usize) -> Vec<String> {
    let mut lines = vec![String::new()];
    let mut used = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if used + cw > w && used > 0 {
            lines.push(String::new());
            used = 0;
        }
        lines.last_mut().unwrap().push(c);
        used += cw;
    }
    lines
}

fn width(s: &str) -> usize {
    s.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// `s` cut to `w` cells, ending with `cut` (one cell: `…` or `~`) if cut.
fn fit_end(s: &str, w: usize, cut: &str) -> String {
    if width(s) <= w {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if used + cw + 1 > w {
            break;
        }
        out.push(c);
        used += cw;
    }
    if w > 0 {
        out.push_str(cut);
    }
    out
}

/// `s` cut to `w` cells in the middle (paths keep their start and their name).
fn fit_middle(s: &str, w: usize, cut: &str) -> String {
    if width(s) <= w || w < 5 {
        return fit_end(s, w, cut);
    }
    let keep = w - 1;
    let tail_w = keep / 2;
    let mut tail: Vec<char> = Vec::new();
    let mut used = 0;
    for c in s.chars().rev() {
        let cw = c.width().unwrap_or(0);
        if used + cw > tail_w {
            break;
        }
        tail.push(c);
        used += cw;
    }
    tail.reverse();
    let head = fit_end(s, keep - used + 1, cut);
    let head = head.strip_suffix(cut).unwrap_or(&head);
    format!("{head}{cut}{}", tail.into_iter().collect::<String>())
}

/// A thin bar `w` cells long, `share` (0 to 1) of it filled in half cells: the filled part and
/// the track after it.
fn bar(share: f64, w: u16, g: &Glyphs) -> (String, String) {
    let [whole, half, track] = g.bar;
    let halves = (share.clamp(0.0, 1.0) * f64::from(w) * 2.0).round() as usize;
    let mut fill = whole.repeat(halves / 2);
    if halves % 2 == 1 {
        fill.push_str(half);
    }
    let rest = track.repeat((w as usize).saturating_sub(halves.div_ceil(2)));
    (fill, rest)
}

/// A share (0 to 1) as a whole percentage: "0%" for nothing, "<1%" for less than half of one,
/// ">99%" for nearly all, so only all of it is "100%".
fn percent(share: f64) -> String {
    let share = share.clamp(0.0, 1.0);
    if share <= 0.0 {
        "0%".into()
    } else if share < 0.005 {
        "<1%".into()
    } else if (0.995..1.0).contains(&share) {
        ">99%".into()
    } else {
        format!("{}%", (share * 100.0).round() as u32)
    }
}

/// A bar at (x, y) on `style`'s background: the filled part in the bar colour, the track dim.
fn draw_bar(buf: &mut Buffer, x: u16, y: u16, share: f64, w: u16, style: Style, look: &Look) {
    let (fill, rest) = bar(share, w, look.g);
    let fw = width(&fill) as u16;
    put(buf, x, y, &fill, w, style.fg(look.c.bar));
    put(
        buf,
        x + fw,
        y,
        &rest,
        w.saturating_sub(fw),
        style.fg(look.c.bar_track),
    );
}

/// An icon and the space after it, or nothing for no icon.
fn icon_gap(icon: &str) -> String {
    if icon.is_empty() {
        String::new()
    } else {
        format!("{icon} ")
    }
}

/// `s` at (x, y), at most `w` cells, cut at the buffer's edge.
fn put(buf: &mut Buffer, x: u16, y: u16, s: &str, w: u16, style: Style) {
    let a = buf.area;
    if x < a.x || x >= a.right() || y < a.y || y >= a.bottom() {
        return;
    }
    buf.set_stringn(x, y, s, w.min(a.right() - x) as usize, style);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::IconSet;

    #[test]
    fn text_fits_its_cells() {
        assert_eq!(fit_end("abcdef", 4, "…"), "abc…");
        assert_eq!(fit_end("abcdef", 4, "~"), "abc~");
        assert_eq!(fit_end("abc", 4, "…"), "abc");
        assert_eq!(width(&fit_end("檔案名稱很長", 7, "…")), 7);
        let p = fit_middle("/home/me/a/very/long/path/name.txt", 20, "…");
        assert_eq!(width(&p), 20);
        assert!(p.starts_with("/home") && p.ends_with("name.txt"), "{p}");
        assert_eq!(clean("caf\n x"), "caf? x");
        let (unicode, ascii) = (IconSet::Unicode.glyphs(), IconSet::Ascii.glyphs());
        assert_eq!(bar(0.5, 4, unicode), ("━━".to_string(), "──".to_string()));
        assert_eq!(bar(0.3, 5, ascii), ("=-".to_string(), "   ".to_string()));
        let (f, r) = bar(0.33, 10, unicode);
        assert_eq!(width(&f) + width(&r), 10);
    }

    #[test]
    fn shares_show_as_whole_percentages() {
        let p = [0.0, 0.0049, 0.005, 0.07, 0.42, 0.994, 0.995, 0.9999, 1.0].map(percent);
        let want = [
            "0%", "<1%", "1%", "7%", "42%", "99%", ">99%", ">99%", "100%",
        ];
        assert_eq!(p, want);
    }

    #[test]
    fn classes_come_from_the_name_and_the_time_only() {
        let k = |name: &str| kind(name.as_bytes(), false);
        assert_eq!(kind(b"photos.zip", true), 0, "a folder is a folder");
        assert_eq!((k("trip.MP4"), k("a.b.png"), k("song.flac")), (1, 2, 3));
        assert_eq!((k("x.tar.gz"), k("cv.pdf"), k("main.rs")), (4, 5, 6));
        assert_eq!(
            (k(".bashrc"), k("README"), k("odd."), k("x.verylongext")),
            (7, 7, 7, 7)
        );
        const DAY: i64 = 86_400;
        let now = 1_000 * DAY;
        let ages = [now + 5, now - 6 * DAY, now - 29 * DAY, now - 364 * DAY, 0];
        assert_eq!(ages.map(|t| age(t, now)), [0, 0, 1, 2, 3]);
        assert_eq!(slider(10, 100, 10, 0, 90), (0, 1));
        assert_eq!(slider(10, 100, 10, 90, 90), (9, 1));
        assert_eq!(slider(10, 20, 10, 5, 10), (3, 5));
    }

    #[test]
    fn names_show_what_they_are_and_cannot_reorder_text() {
        let raw = OsStr::from_bytes(b"a\\b\tc\x1b\xe2\x80\xaed\xe2\x81\xa6e\x7f\xff\xfe.txt");
        assert_eq!(
            shown(raw),
            "a\\\\b\\tc\\x1b\\u{202e}d\\u{2066}e\\x7f\\xff\\xfe.txt"
        );
        assert_eq!(shown(OsStr::new("檔案 ok")), "檔案 ok");
        assert_eq!(clean("x\u{202e}y\u{200f}z\n"), "x?y?z?");
        assert_eq!(wrap("abcdefg", 3), ["abc", "def", "g"]);
        assert_eq!(wrap("檔案名", 4), ["檔案", "名"]);
    }
}
