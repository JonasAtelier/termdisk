// SPDX-License-Identifier: GPL-3.0-only

//! Drawing the view into a ratatui `Buffer`: cells, colours and text, nothing else.

use std::collections::HashSet;
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
use crate::tree::{NodeId, Tree, HARDLINK_DUP, LINKED, LOCKED};
use crate::treemap::{self, Item};
use crate::view::{counted, trash_items, Browse, Confirm, Emptying, Field, Hits, Hover};
use crate::view::{Panes, Screen, Start, View};
use crate::{fmt_size, Scan};

/// termdisk's version, at the end of the title bar.
const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The fewest cells a path in the title is cut to before other parts give way.
const TITLE_MIN: usize = 10;
/// The shortest line of the rest's list: a name of a letter, two spaces, the shortest size
/// ("1.0 kB"), two spaces, a share in four cells.
const REST_LINE_LEAST: usize = 15;
/// Usage's bar: its cells, and the fewest it keeps when names need its room.
const BAR: u16 = 8;
const BAR_MIN: u16 = 3;

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
    // The title's parts; termdisk and its version come last. The tree's starts with a path.
    let title_path = matches!(v.screen, Screen::Browse(_) | Screen::Refreshing(..));
    let mut title: Vec<String> = match &mut v.screen {
        Screen::Start(s) => {
            let del = match s.places[s.sel].path == v.policy.trash_dir() {
                true if v.policy.read_only().is_none() => "Del empty the trash  ",
                _ => "",
            };
            hints = format!("{up_down} choose  Enter scan  {del}? help  q quit");
            start(s, &v.policy, body, buf, &mut hits, &look);
            vec!["choose what to look at".into()]
        }
        Screen::Scanning(s) => {
            hints.push_str("Esc stop  q quit");
            scanning(s, v.scan_started.elapsed(), body, buf, &look);
            vec!["scanning".into()]
        }
        // Scanning again: the last tree is gone (two would double the memory), so the scan's
        // card, under the folder's path; the message (what a trash did) stays in the status bar.
        Screen::Refreshing(s, saved) => {
            hints.push_str("scanning again  Esc stop  q quit");
            scanning(s, v.scan_started.elapsed(), body, buf, &look);
            vec![shown(saved.root.as_os_str()), "scanning again".into()]
        }
        Screen::Browse(b) => {
            let read_only = v.policy.read_only().is_some();
            hints = format!("{up_down} move  {right_left} open  Enter zoom  {back} up  ");
            let on_trash = b.tree.path(b.rows[b.cursor].0) == v.policy.trash_dir();
            match (read_only, on_trash) {
                (true, _) => {}
                (false, true) => hints.push_str("Del empty the trash  "),
                (false, false) => hints.push_str("Del trash  "),
            }
            match items_fit {
                true => hints.push_str("v panes  r rescan  q quit"),
                false => hints.push_str(
                    "o open  c copy  a size  t letters  v panes  r rescan  ? help  q quit",
                ),
            }
            if !b.ticked.is_empty() {
                let size = fmt_size(b.ticked_size(v.apparent));
                let del = match read_only {
                    true => String::new(),
                    false => format!("Del trash them  {sep}  "),
                };
                hints = format!("{} ticked, {size}  {sep}  {del}Esc clear", b.ticked.len());
            }
            if b.emptying.is_some() {
                hints = "emptying the trash  Esc stop  q quit".into();
            }
            // Under the dialog nothing takes a click, so nothing there shows hover.
            let panes = Look {
                pointer: look.pointer.filter(|_| b.confirm.is_none()),
                ..look
            };
            let modes = (v.apparent, v.by_age);
            browse(b, modes, v.panes, body, buf, &mut hits, &panes);
            if let Some(e) = &b.emptying {
                emptying(e, v.scan_started.elapsed(), body, buf, &look);
            }
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
            let path = shown(b.tree.path(b.zoom).as_os_str());
            let mut parts = vec![path, format!("{} ({kind})", fmt_size(size))];
            if !b.trash_reaches && !read_only {
                parts.push("look only: on another filesystem than the trash".into());
            }
            parts
        }
        Screen::Gone => Vec::new(),
    };
    title.push(format!("termdisk v{VERSION}"));
    // termcode's title bar: the text in the middle.
    let st = Style::new().bg(c.title_bg).fg(c.title_fg);
    buf.set_style(Rect::new(area.x, area.y, area.width, 1), st);
    // The close button takes the last 3 cells; the title, in the middle, keeps clear of it on
    // both sides.
    let close = Rect::new(area.right() - 3, area.y, 3, 1);
    let room = area.width.saturating_sub(8) as usize;
    let t = fit_title(&title, title_path, sep, room, g.cut);
    let tw = width(&t) as u16;
    put(buf, area.x + (area.width - tw) / 2, area.y, &t, tw, st);
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
    // Over a lock: why it is locked, in a box at that lock (from the scan and the policy only:
    // nothing here waits on the disk).
    let pointed = |r: &Rect| v.pointer.is_some_and(|(x, y)| r.contains((x, y).into()));
    let lock = v.hits.locks.iter().find(|l| pointed(&l.0)).map(|l| l.0);
    if let (Hover::Lock(id), Some(at), Screen::Browse(b)) = (v.hover, lock, &v.screen) {
        let why = crate::view::lock_reason(&b.tree, id, &v.policy);
        tooltip(&why, at, area, buf, &look);
    }
}

/// `text` in a framed box under `anchor` (over it where there is no room below), from its
/// column or as far right as it fits, inside `area`. The frame shows it in 16 colours too,
/// where the box is as black as the pane.
fn tooltip(text: &str, anchor: Rect, area: Rect, buf: &mut Buffer, look: &Look) {
    let text = clean(text);
    let w = (width(&text) as u16 + 4).min(area.width);
    if w < 6 || area.height < 3 {
        return;
    }
    let x = anchor.x.min(area.right() - w).max(area.x);
    let y = match anchor.y + 4 <= area.bottom() {
        true => anchor.y + 1,
        false => anchor.y.saturating_sub(3).max(area.y),
    };
    let r = Rect::new(x, y, w, 3);
    let style = Style::new().bg(look.c.widget_bg).fg(look.c.widget_fg);
    frame(r, "", style, look, buf);
    let text = fit_end(&text, w as usize - 4, look.g.cut);
    put(buf, r.x + 2, r.y + 1, &text, w - 4, style);
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
fn scanning(s: &Scan, since: Duration, area: Rect, buf: &mut Buffer, look: &Look) {
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
}

/// The trash being emptied: a card over the tree (which waits meanwhile) with a spinner and
/// what has gone so far. Drawn every 100 ms (`View::tick`), as a scan's card.
fn emptying(e: &Emptying, since: Duration, area: Rect, buf: &mut Buffer, look: &Look) {
    use std::sync::atomic::Ordering::Relaxed;
    let (c, g) = (&look.c, look.g);
    let spin = g.spin[(since.as_millis() / 100) as usize % g.spin.len()];
    let (items, bytes) = (e.tally.items.load(Relaxed), e.tally.bytes.load(Relaxed));
    let done = format!(
        "{spin} {items} of {} deleted  {}  {} freed",
        count(e.of as u64, "item"),
        g.dot,
        fmt_size(bytes)
    );
    let stop = match e.tally.stop.load(Relaxed) {
        true => "Stopping: the item it is at stays in the trash, partly deleted",
        false => "Esc stops it: the item it is at stays in the trash, partly deleted",
    };
    let card = Rect::new(
        area.x + area.width.saturating_sub(72) / 2,
        area.y + area.height.saturating_sub(6) / 2,
        area.width.min(72),
        6.min(area.height),
    );
    let style = Style::new().bg(c.widget_bg).fg(c.widget_fg);
    frame(card, " Emptying the trash ", style, look, buf);
    let (x, w) = (card.x + 2, card.width.saturating_sub(4));
    put(
        buf,
        x,
        card.y + 2,
        &done,
        w,
        style.add_modifier(Modifier::BOLD),
    );
    put(buf, x, card.y + 3, stop, w, style.fg(c.dim));
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
    let rows = Rect::new(r.x, r.y + 1, r.width, r.height.saturating_sub(1));
    let h = rows.height as usize;
    b.page = h.max(1);
    if b.cursor < b.top {
        b.top = b.cursor;
    } else if b.cursor >= b.top + h {
        b.top = b.cursor + 1 - h;
    }
    // Never scrolled past where the rows fill the pane (a scan again that keeps the selected
    // row on its line may ask for that): no empty lines below while rows above are hidden.
    b.top = b.top.min(b.rows.len().saturating_sub(h.max(1)));
    // Once a frame, not a walk below each row: a drag can tick thousands.
    let part = b.partly_ticked();
    let shown_rows: Vec<(usize, NodeId, u16, Label)> = (b.rows.iter().enumerate())
        .skip(b.top)
        .take(h)
        .map(|(i, &(id, depth))| (i, id, depth, label(b, id, depth, &part, g)))
        .collect();
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
    // while another can go, and goes too where it alone has no room. Which columns show is
    // decided at their full widths, so it does not change as rows scroll. Usage is a
    // percentage, a space and a bar.
    let mut cols: Vec<(&str, u16)> = vec![
        ("Usage", 5 + BAR),
        ("Size", 9),
        ("Items", 7),
        ("Modified", 10),
    ];
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
    // Then each column takes only what its heading and the rows on screen need (the bar down
    // to `BAR_MIN`), and the cells saved go to the names first, as far as the rows on screen
    // need them, then back to the bar. The scan root's row, its whole path, is left out: the
    // title shows that path.
    let full: u16 = cols.iter().map(|c| c.1 + 1).sum();
    let heading_w = |title: &str| title.len() as u16 + u16::from(sorted(title));
    let (mut size_w, mut items_w, mut name_need) = (heading_w("Size"), heading_w("Items"), 0);
    for (_, id, _, l) in &shown_rows {
        let n = b.tree.node(*id);
        let size = fmt_size(if apparent { n.apparent } else { n.alloc });
        size_w = size_w.max(width(&size) as u16);
        if n.is_dir() {
            items_w = items_w.max(b.tree.items(*id).to_string().len() as u16);
        }
        if *id != 0 {
            name_need = name_need.max(width(&l.full) as u16 + 2);
        }
    }
    for (title, w) in &mut cols {
        *w = (*w).min(match *title {
            "Usage" => 5 + BAR_MIN,
            "Size" => size_w,
            "Items" => items_w,
            _ => *w,
        });
    }
    let saved = full - cols.iter().map(|c| c.1 + 1).sum::<u16>();
    let to_names = saved.min(name_need.saturating_sub(r.width.saturating_sub(full)));
    if let Some(usage) = cols.iter_mut().find(|c| c.0 == "Usage") {
        usage.1 += (saved - to_names).min(BAR - BAR_MIN);
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
    hits.rows = rows;
    hits.scrollbar = Rect::new(bar_x, rows.y, 1, rows.height);
    for (i, id, depth, l) in shown_rows {
        let y = rows.y + (i - b.top) as u16;
        let n = b.tree.node(id);
        let selected = i == b.cursor;
        let locked = l.locked;
        let ticked = b.shows_ticked(id);
        // The selection: its colours, bold, and a mark at the left edge (no colour needed). A
        // ticked row: its tint, under the tick.
        let bg = match (look.over(Rect::new(r.x, y, r.width, 1)), ticked) {
            (true, _) => c.hover_bg,
            (false, true) => c.tick_bg,
            (false, false) => c.side_bg,
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
        let Label {
            indent,
            chev,
            icon,
            boxed,
            mark,
            lead,
            name,
            full,
            levels,
            ..
        } = l;
        let room = name_w.saturating_sub(2) as usize;
        let shown = fit_end(&full, room, g.cut);
        put(buf, r.x + 1, y, &shown, name_w, style);
        // Only as far as the text is shown, not over where it is cut.
        let cut = usize::from(width(&full) > room);
        let end = r.x + 1 + width(&shown).saturating_sub(cut) as u16;
        let x = r.x + 1 + width(&indent) as u16;
        // The tick box, drawn whole: ticked or holding ticks in the tick's colour, else dim
        // (the selected row keeps its own); underlined under the pointer. A click on it ticks
        // the row, but on the shown folder's row (a box there only says it is ticked).
        let at = Rect::new(x + 2, y, width(icon) as u16, 1);
        if boxed && at.right() <= end {
            let st = match (selected, mark) {
                (true, _) => style,
                (false, 0) => style.fg(c.dim),
                (false, _) => style.fg(c.tick_fg),
            };
            let st = match look.over(at) {
                true => st.add_modifier(Modifier::UNDERLINED),
                false => st,
            };
            put(buf, at.x, y, icon, at.width, st);
            if id != b.zoom {
                hits.boxes.push((at, id));
            }
        }
        // Each level's colour on its guide, and on the chevron and folder icon of a row at that
        // level (the selected row keeps its own).
        if !selected {
            let mut paint = |x: u16, glyph: &str, fg: Color| {
                if x < end {
                    put(buf, x, y, glyph, 1, style.fg(fg));
                }
            };
            let level = |d: u16| c.depth[usize::from(d) % c.depth.len()];
            for d in 0..levels {
                paint(r.x + 1 + 2 * d, g.guide, level(d));
            }
            let fg = if n.is_dir() { level(depth) } else { c.icon_fg };
            paint(x, chev, fg);
            if !icon.is_empty() && !boxed {
                paint(x + 2, icon, fg);
            }
        }
        if locked && width(&full) <= room {
            let x = r.x + 1 + width(&lead) as u16 + width(&name) as u16 + 1;
            put(buf, x, y, g.lock, 1, style.fg(c.lock));
            hits.locks.push((Rect::new(x - 1, y, 2, 1), id));
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
                    let t = format!("{:>w$}", b.tree.items(id), w = *w as usize);
                    put(buf, x + 1, y, &t, *w, quiet);
                }
                "Modified" => {
                    let t = &sys::local_datetime(n.mtime.into())[..10];
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

/// A tree row's text, termcode's row: [indent][chevron][space][icon][space][name], then the lock
/// and the hard-link note; each level of the indent is a guide and a space.
struct Label {
    indent: String,
    chev: &'static str,
    icon: &'static str,
    /// The icon is the row's tick box, and which (`Glyphs::boxes`).
    boxed: bool,
    mark: usize,
    /// The indent, the chevron and the icon, with their spaces.
    lead: String,
    name: String,
    full: String,
    levels: u16,
    locked: bool,
}

/// `part`: the folders holding a ticked item (`Browse::partly_ticked`).
fn label(b: &Browse, id: NodeId, depth: u16, part: &HashSet<NodeId>, g: &'static Glyphs) -> Label {
    let n = b.tree.node(id);
    let locked = n.flags & LOCKED != 0;
    let open = b.shows_open(id);
    let (chev, icon) = match (n.is_dir(), open) {
        (true, true) => (g.open, g.folder_open),
        (true, false) => (g.closed, g.folder),
        _ => (" ", ""), // a file's row has its tick box
    };
    // Every row but the shown folder's has its tick box in its icon's place: ticked (itself or
    // with a folder above), else holding a ticked item, else not. The shown folder's row keeps
    // its icon unless it is ticked (it takes no tick, but may lie in a ticked folder).
    let mark = match b.shows_ticked(id) {
        true => 1,
        false => 2 * usize::from(part.contains(&id)),
    };
    let boxed = id != b.zoom || mark == 1;
    let icon = match boxed {
        true => g.boxes[mark],
        false => icon,
    };
    let levels = depth.min(12);
    let indent = format!("{} ", g.guide).repeat(levels as usize);
    let lead = format!("{indent}{chev} {}", icon_gap(icon));
    let mut name = name_of(b, id);
    if n.is_dir() && id != 0 {
        name.push('/');
    }
    let lock = match locked {
        true => format!(" {}", g.lock),
        false => String::new(),
    };
    let link = match n.flags & (LINKED | HARDLINK_DUP) != 0 {
        true => " (hard link)",
        false => "",
    };
    let full = format!("{lead}{name}{lock}{link}");
    Label {
        indent,
        chev,
        icon,
        boxed,
        mark,
        lead,
        name,
        full,
        levels,
        locked,
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
/// sizes, the letters by age). Every tile shows its class as a letter too, its whole name, its
/// size and its share of the folder, never cut (the layout gives it the columns), and has a
/// frame where it has room (white on the selected one, which also has the mark in its
/// corner): all inside the tile, so a click on them picks that tile. The "+N items" tile lists
/// what it holds, each line whole; a click there picks the tile (the lines take no clicks of
/// their own).
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
    let b: &Browse = b;
    let sizes: Vec<u64> = kids
        .iter()
        .map(|&k| counted(b.tree.node(k), apparent))
        .collect();
    let selected = selected.and_then(|s| kids.iter().position(|&k| k == s));
    // Each item's share of the folder on the map, as its row's Usage column shows it.
    let whole = counted(b.tree.node(folder), apparent).max(1) as f64;
    let share = |k: usize| percent(sizes[k] as f64 / whole);
    let detail = |k: usize| format!("{}  {}", fmt_size(sizes[k]), share(k));
    // The columns a tile's whole text takes, never cut: the class letter and the name on the
    // frame's top edge, the size and share under it; the frame's two sides and the gutter.
    // The rest's: "+N", where it shares a strip.
    let cols = |item: Item| {
        let text = match item {
            Item::One(k) => (2 + width(&tile_name(b, kids[k], g))).max(width(&detail(k))),
            Item::Rest(n) => width(&format!("+{n}")),
        };
        u16::try_from(text + 3).unwrap_or(u16::MAX)
    };
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
    let tiles = treemap::layout(&sizes, r, cols);
    // The "+N items" tile holds every item not empty without a tile of its own: the smallest,
    // and any whose text is wider than the map.
    let mut own = vec![false; kids.len()];
    for t in &tiles {
        if let Item::One(k) = t.item {
            own[k] = true;
        }
    }
    let in_rest = |k: usize| sizes[k] > 0 && !own[k];
    let mut shown = [false; 8];
    for t in tiles {
        let rect = t.rect;
        // A gap at the right and bottom keeps tiles of one colour apart.
        let fill = Rect::new(
            rect.x,
            rect.y,
            rect.width - u16::from(rect.width > 3),
            rect.height - u16::from(rect.height > 2),
        );
        let (id, label, detail) = match t.item {
            Item::One(k) => {
                let id = kids[k];
                let n = b.tree.node(id);
                let class = match by_age {
                    true => age(n.mtime.into(), now),
                    false => kind(b.tree.name(id).as_bytes(), n.is_dir()),
                };
                shown[class] = true;
                let label = format!("{} {}", classes[class].0, tile_name(b, id, g));
                (Some(id), label, detail(k))
            }
            Item::Rest(_) => (None, String::new(), String::new()),
        };
        let chosen = selected.is_some_and(|s| match t.item {
            Item::One(k) => k == s,
            Item::Rest(_) => in_rest(s),
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
        // The gutter row under the tile keeps the tile's colour in its upper half where the set
        // has the glyph: half a row looks as wide as the one-column gap at the right.
        if fill.height < rect.height && !g.half_row.is_empty() {
            let gutter = Style::new().bg(c.bg).fg(style.bg.unwrap_or(c.bg));
            for x in fill.x..fill.right() {
                put(buf, x, fill.bottom(), g.half_row, 1, gutter);
            }
        }
        // Every tile has a frame where it has room (the treemap gives every tile of its own
        // that room): dark on the others, white on the selected. The name sits on its top edge
        // and the size under it; the rest has no size, so two rows hold its frame.
        let outlined = fill.width >= 4 && fill.height >= if detail.is_empty() { 2 } else { 3 };
        if outlined {
            let edge = if chosen { style } else { style.fg(c.bg) };
            outline(fill, edge, look, buf);
        }
        let w = fill.width.saturating_sub(1 + u16::from(outlined));
        // The text is bold on every tile; the selected one shows by its colour, frame and mark.
        let bold = style.add_modifier(Modifier::BOLD);
        let top = match look.over(rect) && !chosen {
            true => bold.add_modifier(Modifier::UNDERLINED),
            false => bold,
        };
        // A tile of its own has room for its whole text (`cols`). The rest's "+N items", or
        // where that has no room "+N"; without a frame it starts at the tile's edge, under the
        // selection's mark if that takes the first cell.
        let (mut x, mut y, mut room) = (fill.x + 1, fill.y, w);
        if let (Item::Rest(_), false) = (t.item, outlined) {
            (x, room) = (fill.x, fill.width);
            if chosen && fill.height > 1 {
                y += 1;
            } else if chosen {
                (x, room) = (fill.x + 1, w);
            }
        }
        let label = match t.item {
            Item::One(_) => label,
            Item::Rest(count) => rest_label(count, room as usize),
        };
        put(buf, x, y, &label, room, top);
        // The lock ends the label: the pointer there says why.
        if let Some(i) = id.filter(|&i| b.tree.node(i).flags & LOCKED != 0) {
            let x = fill.x + width(&label) as u16; // the label's last cell
            hits.locks.push((Rect::new(x - 1, fill.y, 2, 1), i));
        }
        if fill.height > 1 && !detail.is_empty() {
            put(buf, fill.x + 1, fill.y + 1, &detail, w, bold);
        }
        if let (Item::Rest(count), true) = (t.item, outlined) {
            let rest = (0..kids.len()).filter(|&k| in_rest(k));
            let inside = Rect::new(fill.x + 1, fill.y + 1, w, fill.height.saturating_sub(2));
            let lines = |k: usize| (tile_name(b, kids[k], g), fmt_size(sizes[k]), share(k));
            let plain = style.remove_modifier(Modifier::BOLD);
            rest_list(rest, count, lines, selected, inside, plain, buf);
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

/// An item's name as its tile and the rest's list show it: `/` after a folder, the lock after
/// what cannot be trashed itself.
fn tile_name(b: &Browse, id: NodeId, g: &Glyphs) -> String {
    let n = b.tree.node(id);
    let mut name = name_of(b, id);
    if n.is_dir() {
        name.push('/');
    }
    if n.flags & LOCKED != 0 {
        name.push(' ');
        name.push_str(g.lock);
    }
    name
}

/// The rest's label in `room` cells, never cut: "+N items" ("+1 item"), else "+N", else
/// nothing ("+1" of "+12" would say one item).
fn rest_label(count: usize, room: usize) -> String {
    let full = match count {
        1 => "+1 item".to_string(),
        n => format!("+{n} items"),
    };
    let short = format!("+{count}");
    match room {
        r if full.len() <= r => full,
        r if short.len() <= r => short,
        _ => String::new(),
    }
}

/// The list inside the rest's frame, over `inside`: what it holds (`rest`, `count` of them),
/// largest first, a line each: the name at the left, the size and the share at the right
/// (`lines`). A line shows only whole: an item whose name, size and share do not fit is told in
/// "+N more", with those the rows have no room for; that line too shows only whole, and under
/// one item at least (alone it would say the label again). The selected item's line is
/// reversed, so it shows without colour too. Only a few more items than rows are looked at
/// (each costs a name, a size and a share, and a folder may hand 166 000 to every drawing);
/// "+N more" counts all the others.
fn rest_list(
    rest: impl Iterator<Item = usize>,
    count: usize,
    lines: impl Fn(usize) -> (String, String, String),
    selected: Option<usize>,
    inside: Rect,
    style: Style,
    buf: &mut Buffer,
) {
    let (room, rows) = (inside.width as usize, usize::from(inside.height));
    if rows == 0 || room < REST_LINE_LEAST {
        return;
    }
    let line = |k: usize| {
        let (name, size, share) = lines(k);
        let right = format!("{size}  {share:>4}");
        (width(&name) + 2 + width(&right) <= room).then_some((name, right))
    };
    let mut whole: Vec<(usize, String, String)> = Vec::new();
    for k in rest.take(4 * rows + 16) {
        if whole.len() > rows {
            break;
        }
        if let Some((name, right)) = line(k) {
            whole.push((k, name, right));
        }
    }
    let listed = match whole.len() == count && count <= rows {
        true => count,
        false => whole.len().min(rows.saturating_sub(1)),
    };
    let more = format!("+{} more", count - listed);
    if listed == 0 || (listed < count && width(&more) > room) {
        return;
    }
    for (row, (k, name, right)) in whole.iter().take(listed).enumerate() {
        let y = inside.y + row as u16;
        let st = match selected == Some(*k) {
            true => style.add_modifier(Modifier::REVERSED),
            false => style,
        };
        buf.set_style(Rect::new(inside.x, y, inside.width, 1), st);
        put(buf, inside.x, y, name, inside.width, st);
        let right_w = width(right) as u16;
        put(buf, inside.right() - right_w, y, right, right_w, st);
    }
    if listed < count {
        put(
            buf,
            inside.x,
            inside.y + listed as u16,
            &more,
            inside.width,
            style,
        );
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
/// cannot hold it all, it says so, and the item cannot be moved (`Confirm::fits`). For several
/// items: `dialog_many`.
fn dialog(
    c: &mut Confirm,
    tree: &Tree,
    apparent: bool,
    area: Rect,
    buf: &mut Buffer,
    hits: &mut Hits,
    look: &Look,
) {
    if c.empty {
        return dialog_empty(c, tree, area, buf, hits, look);
    }
    if !c.one() {
        return dialog_many(c, tree, apparent, area, buf, hits, look);
    }
    let (k, g) = (&look.c, look.g);
    let (id, path) = &c.items[0];
    let n = tree.node(*id);
    let linked = n.flags & (LINKED | HARDLINK_DUP) != 0;
    let w = area.width.saturating_sub(2).min(80);
    let iw = w.saturating_sub(4);
    let lines = wrap(&shown(path.as_os_str()), iw as usize);
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
    // From the ticks: the item is the ticked one, not the selected row.
    let title = match c.ticked {
        true => " Move the ticked item to the trash? ",
        false => " Move to the trash? ",
    };
    frame(r, title, style, look, buf);
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
            format!(
                "{size}  {}  {}",
                g.dot,
                count(tree.items(*id).into(), "item")
            )
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
    buttons(c, x, y + 2, buf, hits, look);
}

/// The dialog to empty the trash: how many items and how much disk, its whole path, and that
/// they go for good. As the trash dialog, nothing goes while the window cannot show it all.
fn dialog_empty(
    c: &mut Confirm,
    tree: &Tree,
    area: Rect,
    buf: &mut Buffer,
    hits: &mut Hits,
    look: &Look,
) {
    let k = &look.c;
    let (id, path) = &c.items[0];
    let w = area.width.saturating_sub(2).min(80);
    let iw = w.saturating_sub(4);
    let what = format!(
        "{}, {} on disk, in",
        count(trash_items(tree, *id) as u64, "item"),
        fmt_size(tree.node(*id).alloc)
    );
    let path = wrap(&shown(path.as_os_str()), iw as usize);
    let note = "will be deleted permanently. This cannot be undone.";
    let note = wrap(note, iw as usize);
    // Border, blank, what, the path, the note, blank, buttons, border.
    let full = 6 + (path.len() + note.len()) as u16;
    c.fits = iw >= 33 && full <= area.height;
    let h = if c.fits { full } else { 7 };
    let r = Rect::new(
        area.x + (area.width - w) / 2,
        area.y + area.height.saturating_sub(h) / 2,
        w,
        h,
    );
    let style = Style::new().bg(k.widget_bg).fg(k.widget_fg);
    frame(r, " Empty the trash? ", style, look, buf);
    let x = r.x + 2;
    let mut y = r.y + 2;
    if c.fits {
        let bold = style.add_modifier(Modifier::BOLD);
        put(buf, x, y, &what, iw, bold);
        for line in &path {
            y += 1;
            put(buf, x, y, line, iw, bold);
        }
        for line in &note {
            y += 1;
            put(buf, x, y, line, iw, style);
        }
    } else {
        let note = "The window is too small to show the whole path:";
        put(buf, x, y, note, iw, style);
        y += 1;
        put(buf, x, y, "make it larger, or press Esc.", iw, style);
    }
    buttons(c, x, y + 2, buf, hits, look);
}

/// The dialog for several items (the ticked ones): their count and total, then every path,
/// whole, wrapped and escaped, in a list that scrolls; after them the ticked items the guard
/// refused, each with the lock and why. Move to Trash works only once every path to move has
/// been shown whole (`Confirm::seen`: the lines jumped over by End do not count), and while
/// the dialog has room for the list (`Confirm::fits`).
fn dialog_many(
    c: &mut Confirm,
    tree: &Tree,
    apparent: bool,
    area: Rect,
    buf: &mut Buffer,
    hits: &mut Hits,
    look: &Look,
) {
    let (k, g) = (&look.c, look.g);
    let w = area.width.saturating_sub(2).min(80);
    let iw = w.saturating_sub(4);
    // The list's lines: each with whether it is a path to move (in full colour) and the first
    // line of the entry it is part of; and the lines each path to move takes.
    let mut lines: Vec<(String, bool, usize)> = Vec::new();
    let mut spans = Vec::new();
    for (_, path) in &c.items {
        let start = lines.len();
        let path = wrap(&shown(path.as_os_str()), iw as usize);
        lines.extend(path.into_iter().map(|l| (l, true, start)));
        spans.push(start..lines.len());
    }
    if !c.refused.is_empty() {
        let head = format!("Not moved ({}):", c.refused.len());
        lines.push((head, false, lines.len()));
        for (path, why) in &c.refused {
            let (start, text) = (
                lines.len(),
                format!("{} {}: {}", g.lock, shown(path.as_os_str()), clean(why)),
            );
            lines.extend(
                wrap(&text, iw as usize)
                    .into_iter()
                    .map(|l| (l, false, start)),
            );
        }
    }
    let linked = c.items.iter().any(|(id, _)| {
        let n = tree.node(*id);
        n.flags & (LINKED | HARDLINK_DUP) != 0
    });
    // Border, blank, the total, the list, where in it, (hard links), the trash line, blank,
    // buttons, border.
    let fixed = 8 + u16::from(linked);
    let n_lines = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    let list = area.height.saturating_sub(fixed).min(n_lines);
    let ok = iw >= 33 && list >= n_lines.min(3);
    let h = if ok { fixed + list } else { 7 };
    let r = Rect::new(
        area.x + (area.width - w) / 2,
        area.y + area.height.saturating_sub(h) / 2,
        w,
        h,
    );
    let style = Style::new().bg(k.widget_bg).fg(k.widget_fg);
    let title = format!(
        " Move {} to the trash? ",
        count(c.items.len() as u64, "item")
    );
    frame(r, &title, style, look, buf);
    let x = r.x + 2;
    let mut y = r.y + 2;
    if ok {
        let (mut size, mut held) = (0, 0);
        for (id, _) in &c.items {
            let n = tree.node(*id);
            size += if apparent { n.apparent } else { n.alloc };
            held += u64::from(tree.items(*id));
        }
        let held = count(held, "item");
        let what = format!("{}  {}  {held} in them", fmt_size(size), g.dot);
        put(buf, x, y, &what, iw, style.add_modifier(Modifier::BOLD));
        y += 1;
        let list = usize::from(list);
        c.scroll = c.scroll.min(lines.len() - list);
        c.page = list;
        // The next page starts at the entry the bottom edge cuts, so none is split twice.
        let next = c.scroll + list;
        c.next_page = match lines.get(next) {
            Some(&(_, _, start)) if start > c.scroll => start,
            _ => next,
        };
        for (line, moves, _) in lines.iter().skip(c.scroll).take(list) {
            let st = if *moves { style } else { style.fg(k.dim) };
            put(buf, x, y, line, iw, st);
            y += 1;
        }
        // A path is seen once all its lines were on screen together.
        let on_screen = c.scroll..c.scroll + list;
        for (seen, span) in c.seen.iter_mut().zip(&spans) {
            *seen |= on_screen.start <= span.start && span.end <= on_screen.end;
        }
        let [up_down, ..] = g.keys;
        let mut place = String::new();
        if lines.len() > list {
            let (from, to) = (c.scroll + 1, c.scroll + list);
            place = format!(
                "lines {from}-{to} of {}  {}  {up_down} PgUp PgDn",
                lines.len(),
                g.dot
            );
        }
        if !c.seen.iter().all(|&s| s) {
            place.push_str("  (Move once every path was shown)");
        }
        put(buf, x, y, &place, iw, style.fg(k.dim));
        y += 1;
        if linked {
            let note = "Some have other hard links: moving them may free nothing.";
            put(buf, x, y, note, iw, style);
            y += 1;
        }
        let note = "The space is freed only when the trash is emptied.";
        put(buf, x, y, note, iw, style);
    } else {
        let note = "The window is too small to show every path:";
        put(buf, x, y, note, iw, style);
        y += 1;
        put(buf, x, y, "make it larger, or press Esc.", iw, style);
    }
    c.fits = ok && c.seen.iter().all(|&s| s);
    buttons(c, x, y + 2, buf, hits, look);
}

/// termcode's buttons, Cancel and Move to Trash (Empty Trash), at line `y`: the one with the
/// focus primary (and bold), the other secondary. A Move to Trash that cannot be used yet (or
/// here: a path is not shown) is crossed out; without the focus, dim and grey as well.
fn buttons(c: &Confirm, x: u16, y: u16, buf: &mut Buffer, hits: &mut Hits, look: &Look) {
    let k = &look.c;
    let primary = Style::new()
        .bg(k.button_bg)
        .fg(k.button_fg)
        .add_modifier(Modifier::BOLD);
    let secondary = Style::new().bg(k.button2_bg).fg(k.button2_fg);
    let label = match c.empty {
        true => "[ Empty Trash ]",
        false => "[ Move to Trash ]",
    };
    let cancel = Rect::new(x + 2, y, 10, 1);
    let moving = Rect::new(x + 16, y, width(label) as u16, 1);
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
    // The focus may come before it can be used (Tab works at once): then the focus's colours,
    // crossed out.
    let move_style = if !c.clickable() {
        match c.on_move {
            true => primary.add_modifier(Modifier::CROSSED_OUT),
            false => Style::new()
                .bg(k.button2_bg)
                .fg(k.disabled)
                .add_modifier(Modifier::CROSSED_OUT | Modifier::DIM),
        }
    } else if c.on_move {
        lit(moving, primary, true)
    } else {
        lit(moving, secondary, false)
    };
    put(buf, moving.x, moving.y, label, moving.width, move_style);
    hits.buttons = Some((cancel, moving));
}

fn help(area: Rect, buf: &mut Buffer, look: &Look) {
    let [up_down, right_left, _] = look.g.keys;
    let (moves, opens) = (
        format!("{up_down}  PgUp PgDn"),
        format!("{right_left}  + -"),
    );
    let part = format!("{} (tree)", look.g.boxes[2]);
    let keys: [(&str, &str); 23] = [
        (&moves, "move"),
        (&opens, "open, close a folder"),
        ("double click (tree)", "open, close a folder"),
        ("Enter, double click (map)", "show that folder"),
        ("Backspace", "show the folder above"),
        ("Del", "move to the trash (asks first)"),
        ("Del on the trash", "empty it for good (asks first)"),
        (
            "Space, box, drag (tree)",
            "tick, untick; Del: the ticked ones",
        ),
        (&part, "not ticked, but something in it is"),
        ("o", "open with the default program"),
        ("c", "copy the path"),
        ("a", "disk usage / apparent size"),
        ("t", "map letters: by type / by age"),
        ("s, S", "sort the tree: next column / reverse"),
        ("v", "tree and map / tree / map"),
        ("r", "scan again"),
        ("Esc", "stop a scan, close a dialog, clear the ticks"),
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

/// The title in `room` cells: its parts with `sep` between them. What gives way first is the
/// first part: a path (`path`) is cut in its middle while it keeps `TITLE_MIN` cells, other
/// text shows whole or not at all. Then the parts between it and the last go, from the last
/// one back; then the first part goes; the last (termdisk and its version) is cut only where
/// it alone has no room.
fn fit_title(parts: &[String], path: bool, sep: &str, room: usize, cut: &str) -> String {
    let [first, middle @ .., last] = parts else {
        return parts
            .first()
            .map_or_else(String::new, |p| fit_end(p, room, cut));
    };
    let gap = format!("  {sep}  ");
    let least = match path {
        true => TITLE_MIN.min(width(first)),
        false => width(first),
    };
    for keep in (0..=middle.len()).rev() {
        let tail: String = middle[..keep]
            .iter()
            .chain([last])
            .map(|p| format!("{gap}{p}"))
            .collect();
        if width(&tail) + least <= room {
            let free = room - width(&tail);
            return format!("{}{tail}", fit_middle(first, free, cut));
        }
    }
    fit_end(last, room, cut)
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

/// `n` of `thing`: "1 item", "2 items".
pub(crate) fn count(n: u64, thing: &str) -> String {
    match n {
        1 => format!("1 {thing}"),
        n => format!("{n} {thing}s"),
    }
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
    fn a_reason_box_stays_inside_its_area_at_every_edge() {
        let look = Look {
            c: Colors::default(),
            g: IconSet::Ascii.glyphs(),
            pointer: None,
        };
        let why = "protected: ~/.config, with everything in it"; // 43 cells: wider than the area
        let area = Rect::new(2, 1, 30, 10);
        let (right, bottom) = (area.right() - 2, area.bottom() - 1);
        for (anchor, short) in [
            (Rect::new(2, 1, 2, 1), false),
            (Rect::new(right, 1, 2, 1), false),
            (Rect::new(right, bottom, 2, 1), false),
            (Rect::new(2, bottom, 2, 1), true),
            (Rect::new(12, 5, 2, 1), true),
        ] {
            let mut buf = Buffer::empty(Rect::new(0, 0, 40, 14));
            for c in buf.content.iter_mut() {
                c.set_symbol("X");
            }
            let text = if short { "a mount point: /m" } else { why };
            tooltip(text, anchor, area, &mut buf, &look);
            let at = format!("{anchor:?}");
            for (i, c) in buf.content.iter().enumerate() {
                let (x, y) = ((i % 40) as u16, (i / 40) as u16);
                let inside = x >= area.x && x < area.right() && y >= area.y && y < area.bottom();
                assert!(inside || c.symbol() == "X", "({x}, {y}) for {at}");
            }
            // Under the lock where it has room, else over it; at the lock's column, else as
            // far right as it fits.
            let top = match anchor.y + 4 <= area.bottom() {
                true => anchor.y + 1,
                false => anchor.y - 3,
            };
            let w = (width(text) as u16 + 4).min(area.width);
            let x = anchor.x.min(area.right() - w);
            assert_eq!(buf[(x, top)].symbol(), "+", "{at}");
            let line: String = (x + 2..x + w - 2)
                .map(|x| buf[(x, top + 1)].symbol())
                .collect();
            match short {
                true => assert_eq!(line, text, "{at}"),
                false => assert!(
                    line.starts_with("protected:") && line.ends_with('~'),
                    "{at}"
                ),
            }
        }
    }

    #[test]
    fn the_title_gives_way_path_first_and_the_version_last() {
        let parts = [
            "/home/me/a/long/path/to/photos",
            "4.4 MB (disk usage)",
            "termdisk v9.9.9",
        ];
        let parts: Vec<String> = parts.map(String::from).into();
        let t = |room| fit_title(&parts, true, "-", room, "~");
        let all = "/home/me/a/long/path/to/photos  -  4.4 MB (disk usage)  -  termdisk v9.9.9";
        assert_eq!(t(100), all);
        assert_eq!(t(74), all);
        // The path is cut in its middle first...
        assert_eq!(
            t(60),
            "/home/me~/photos  -  4.4 MB (disk usage)  -  termdisk v9.9.9"
        );
        assert_eq!(width(&t(54)), 54);
        // ...to TITLE_MIN cells; then the size goes, and the path has room again.
        assert_eq!(t(53), "/home/me/a/long/path/to/photos  -  termdisk v9.9.9");
        assert_eq!(width(&t(30)), 30);
        // The version alone, then cut itself only where it has no room.
        assert_eq!(t(29), "termdisk v9.9.9");
        assert_eq!(t(15), "termdisk v9.9.9");
        assert_eq!(t(10), "termdisk ~");
        assert_eq!(fit_title(&parts[2..], true, "-", 10, "~"), "termdisk ~");
        // With a note between the size and the version: the path is cut first, then the parts
        // between go from the last one back, so the size stays longest.
        let mut four = parts.clone();
        four.insert(2, "look only: on another filesystem than the trash".into());
        let t = |room| fit_title(&four, true, "-", room, "~");
        assert_eq!(width(&t(126)), 126);
        assert!(t(126).contains("/home/me/a/long/path/to/photos"));
        let cut = t(112);
        assert!(cut.contains("me~/photos") && cut.contains("look only") && width(&cut) == 112);
        let at_80_cols = t(72);
        assert!(at_80_cols.contains("(disk usage)") && !at_80_cols.contains("look only"));
        assert_eq!(width(&at_80_cols), 72);
        assert_eq!(t(53), "/home/me/a/long/path/to/photos  -  termdisk v9.9.9");
        // Text that is not a path shows whole or not at all.
        let start: Vec<String> = ["choose what to look at", "termdisk v9.9.9"]
            .map(String::from)
            .into();
        let t = |room| fit_title(&start, false, "-", room, "~");
        assert_eq!(t(42), "choose what to look at  -  termdisk v9.9.9");
        assert_eq!(t(41), "termdisk v9.9.9");
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
