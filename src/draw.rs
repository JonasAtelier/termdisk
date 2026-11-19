//! Drawing the view into a ratatui `Buffer`: cells, colours and text, nothing else.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use unicode_width::UnicodeWidthChar;

use crate::guard::Policy;
use crate::sys;
use crate::tree::{Tree, HARDLINK_DUP, LINKED, MOUNT, NOT_OWNED, PROTECTED, UNREADABLE};
use crate::treemap::{self, Item};
use crate::view::{counted, Browse, Confirm, Hits, Panes, Screen, Start, View};
use crate::{fmt_size, Scan};

/// The flags that keep an item from the trash, shown with a lock.
const LOCKED: u16 = PROTECTED | NOT_OWNED | MOUNT | UNREADABLE;
const LOCK: &str = "🔒";
/// Tile backgrounds (256-colour indexes), muted enough for white text.
const PALETTE: [u8; 8] = [24, 29, 54, 94, 23, 58, 89, 60];

fn bar_style() -> Style {
    Style::default()
        .bg(Color::Indexed(236))
        .fg(Color::Indexed(252))
}

fn dim() -> Style {
    Style::default().fg(Color::Indexed(244))
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
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            buf[(x, y)].reset();
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
            Style::default(),
        );
        if let Screen::Browse(b) = &mut v.screen {
            if let Some(c) = &mut b.confirm {
                c.fits = false; // nothing of it is shown, so nothing can be moved
            }
        }
        v.hits = hits;
        return;
    }
    let body = Rect::new(area.x, area.y + 1, area.width, area.height - 2);
    let (status, mut hints) = (area.bottom() - 1, String::new());
    let title = match &mut v.screen {
        Screen::Start(s) => {
            hints.push_str("↑↓ choose  Enter scan  ? help  q quit");
            start(s, &v.policy, body, buf, &mut hits);
            " termdisk  ·  choose what to look at".to_string()
        }
        Screen::Scanning(s) => {
            hints.push_str("Esc stop  q quit");
            scanning(s, body, buf)
        }
        Screen::Browse(b) => {
            let read_only = v.policy.read_only().is_some();
            hints.push_str("↑↓ move  →← open  Enter zoom  ⌫ up  ");
            if !read_only {
                hints.push_str("Del trash  ");
            }
            hints.push_str("o open  c copy  a size  v panes  r rescan  ? help  q quit");
            browse(b, v.apparent, v.panes, body, buf, &mut hits);
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
            let look = if b.trash_reaches || read_only {
                ""
            } else {
                "  ·  look only: on another filesystem than the trash"
            };
            let path = shown(b.tree.path(b.zoom).as_os_str());
            format!(
                " termdisk  ·  {path}  ·  {}  ({kind}){look}",
                fmt_size(size)
            )
        }
        Screen::Gone => String::new(),
    };
    buf.set_style(Rect::new(area.x, area.y, area.width, 1), bar_style());
    put(buf, area.x, area.y, &title, area.width, bar_style());
    let line = match (&v.message, v.policy.read_only()) {
        (Some(m), _) => format!(" {}", clean(m)),
        (None, Some(why)) => format!(" {why}  ·  {hints}"),
        (None, None) => format!(" {hints}"),
    };
    buf.set_style(Rect::new(area.x, status, area.width, 1), bar_style());
    put(buf, area.x, status, &line, area.width, bar_style());
    if let Screen::Browse(b) = &mut v.screen {
        let Browse { tree, confirm, .. } = &mut **b;
        if let Some(c) = confirm {
            dialog(c, tree, v.apparent, area, buf, &mut hits);
        }
    }
    if v.help {
        help(area, buf);
    }
    v.hits = hits;
}

fn start(s: &Start, policy: &Policy, area: Rect, buf: &mut Buffer, hits: &mut Hits) {
    let x = area.x + 2;
    let w = area.width.saturating_sub(4);
    put(
        buf,
        x,
        area.y + 1,
        "Places",
        w,
        Style::default().add_modifier(Modifier::BOLD),
    );
    let list = Rect::new(area.x, area.y + 2, area.width, s.places.len() as u16);
    hits.places = list;
    for (i, p) in s.places.iter().enumerate() {
        let y = list.y + i as u16;
        let style = if i == s.sel {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        let note = if !p.exists {
            "not there"
        } else if !p.trashable && policy.read_only().is_none() {
            "look only: on another filesystem than the trash"
        } else {
            ""
        };
        let path = clean(&p.path.to_string_lossy());
        let text = format!(" {:<16} {path}", p.name);
        buf.set_style(Rect::new(x, y, w, 1), style);
        put(buf, x, y, &text, w, style);
        let nw = note.chars().count() as u16;
        if !note.is_empty() && w > nw + 40 {
            put(buf, x + w - nw - 1, y, note, nw, style.patch(dim()));
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
        put(buf, x, y, &text, w, Style::default());
        let bw = w.saturating_sub(width(&text) as u16).min(30);
        put(buf, x + width(&text) as u16, y, &bar(share, bw), bw, dim());
    }
}

fn scanning(s: &Scan, area: Rect, buf: &mut Buffer) -> String {
    use std::sync::atomic::Ordering::Relaxed;
    let p = s.progress();
    let (x, w) = (area.x + 2, area.width.saturating_sub(4));
    let line = format!(
        "{} files  ·  {} folders  ·  {}",
        p.files.load(Relaxed),
        p.dirs.load(Relaxed),
        fmt_size(p.bytes.load(Relaxed))
    );
    put(
        buf,
        x,
        area.y + 1,
        &line,
        w,
        Style::default().add_modifier(Modifier::BOLD),
    );
    let errors = p.errors.load(Relaxed);
    if errors > 0 {
        put(
            buf,
            x,
            area.y + 2,
            &format!("{errors} could not be read"),
            w,
            dim(),
        );
    }
    let current = p.current.lock().map(|c| c.clone()).unwrap_or_default();
    let current = clean(&current.to_string_lossy());
    put(
        buf,
        x,
        area.y + 3,
        &fit_middle(&current, w as usize),
        w,
        dim(),
    );
    " termdisk  ·  scanning".to_string()
}

fn browse(
    b: &mut Browse,
    apparent: bool,
    panes: Panes,
    area: Rect,
    buf: &mut Buffer,
    hits: &mut Hits,
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
        tree_pane(b, apparent, r, buf, hits);
    }
    if let Some(r) = map_area {
        map_pane(b, apparent, r, buf, hits);
    }
}

fn tree_pane(b: &mut Browse, apparent: bool, r: Rect, buf: &mut Buffer, hits: &mut Hits) {
    // In a narrow pane columns go, Items first, so names keep 24 cells.
    let mut cols: Vec<(&str, u16)> =
        vec![("Usage", 10), ("Size", 9), ("Items", 7), ("Modified", 10)];
    for drop in ["Items", "Modified", "Usage", "Size"] {
        if r.width >= 24 + cols.iter().map(|c| c.1 + 1).sum::<u16>() {
            break;
        }
        cols.retain(|c| c.0 != drop);
    }
    let right: u16 = cols.iter().map(|c| c.1 + 1).sum();
    let name_w = r.width.saturating_sub(right);
    let head = Style::default()
        .add_modifier(Modifier::BOLD)
        .fg(Color::Indexed(250));
    put(buf, r.x + 1, r.y, "Name", name_w, head);
    let mut x = r.x + name_w;
    for (title, w) in &cols {
        let t = format!("{title:>w$}", w = *w as usize);
        put(buf, x + 1, r.y, &t, *w, head);
        x += w + 1;
    }
    let rows = Rect::new(r.x, r.y + 1, r.width, r.height.saturating_sub(1));
    hits.rows = rows;
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
        let mut style = Style::default();
        if n.is_dir() {
            style = style.add_modifier(Modifier::BOLD);
        }
        if n.flags & LOCKED != 0 {
            style = style.fg(Color::Indexed(245));
        }
        if selected {
            style = style.add_modifier(Modifier::REVERSED);
            buf.set_style(Rect::new(r.x, y, r.width, 1), style);
        }
        let marker = match (n.is_dir(), b.open.contains(&id) || id == b.zoom) {
            (true, true) => "▾ ",
            (true, false) => "▸ ",
            _ => "  ",
        };
        let mut name = format!(
            "{}{marker}{}",
            "  ".repeat(depth.min(12) as usize),
            name_of(b, id)
        );
        if n.is_dir() && id != 0 {
            name.push('/');
        }
        if n.flags & LOCKED != 0 {
            name.push(' ');
            name.push_str(LOCK);
        }
        if n.flags & (LINKED | HARDLINK_DUP) != 0 {
            name.push_str(" (hard link)");
        }
        put(
            buf,
            r.x + 1,
            y,
            &fit_end(&name, name_w.saturating_sub(2) as usize),
            name_w,
            style,
        );
        let parent = b
            .tree
            .parent(id)
            .filter(|_| id != b.zoom)
            .map(|p| *b.tree.node(p));
        let mut x = r.x + name_w;
        for (title, w) in &cols {
            let text = match *title {
                "Usage" => {
                    let total = parent.map_or(counted(n, apparent), |p| counted(&p, apparent));
                    bar(counted(n, apparent) as f64 / total.max(1) as f64, *w)
                }
                "Size" => {
                    let s = fmt_size(if apparent { n.apparent } else { n.alloc });
                    format!("{s:>w$}", w = *w as usize)
                }
                "Items" if n.is_dir() => format!("{:>w$}", n.items, w = *w as usize),
                "Modified" => sys::local_datetime(n.mtime)[..10].to_string(),
                _ => String::new(),
            };
            put(buf, x + 1, y, &text, *w, style);
            x += w + 1;
        }
    }
}

fn map_pane(b: &mut Browse, apparent: bool, r: Rect, buf: &mut Buffer, hits: &mut Hits) {
    let zoom = b.zoom;
    let kids = b.kids(zoom).to_vec();
    let sizes: Vec<u64> = kids
        .iter()
        .map(|&k| counted(b.tree.node(k), apparent))
        .collect();
    let selected = b.tile_of(b.rows[b.cursor].0);
    for (i, t) in treemap::layout(&sizes, r).into_iter().enumerate() {
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
                let mut name = name_of(b, id);
                if n.is_dir() {
                    name.push('/');
                }
                if n.flags & LOCKED != 0 {
                    name.push_str(LOCK);
                }
                (Some(id), name, fmt_size(sizes[k]))
            }
            Item::Rest(count) => (None, format!("+{count} items"), String::new()),
        };
        let mut style = match id {
            Some(_) => Style::default()
                .bg(Color::Indexed(PALETTE[i % PALETTE.len()]))
                .fg(Color::White),
            None => Style::default()
                .bg(Color::Indexed(239))
                .fg(Color::Indexed(252)),
        };
        if id.is_some() && id == selected {
            style = Style::default()
                .bg(Color::Indexed(220))
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD);
        }
        buf.set_style(fill, style);
        let w = fill.width.saturating_sub(1);
        put(
            buf,
            fill.x + 1,
            fill.y,
            &fit_end(&label, w as usize),
            w,
            style,
        );
        if fill.height > 1 {
            put(buf, fill.x + 1, fill.y + 1, &detail, w, style);
        }
        hits.tiles.push((rect, id));
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
) {
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
    let style = Style::default().bg(Color::Indexed(235)).fg(Color::White);
    frame(r, " Move to the trash? ", style, buf);
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
            format!("{size}  ·  {} items", n.items)
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
    let armed = c.armed() && c.fits;
    let focus = |on: bool| match on {
        true => Style::default()
            .bg(Color::Indexed(220))
            .fg(Color::Black)
            .add_modifier(Modifier::BOLD),
        false => style,
    };
    let cancel = Rect::new(x + 2, y, 10, 1);
    let moving = Rect::new(x + 16, y, 17, 1);
    put(
        buf,
        cancel.x,
        cancel.y,
        "[ Cancel ]",
        cancel.width,
        focus(!c.on_move),
    );
    let move_style = if armed {
        focus(c.on_move)
    } else {
        style.fg(Color::Indexed(242))
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

fn help(area: Rect, buf: &mut Buffer) {
    const KEYS: [(&str, &str); 14] = [
        ("↑ ↓  PgUp PgDn", "move"),
        ("→ ←  + -", "open, close a folder"),
        ("Enter, double click", "show that folder"),
        ("Backspace", "show the folder above"),
        ("Del", "move to the trash (asks first)"),
        ("o", "open with the default program"),
        ("c", "copy the path"),
        ("a", "disk usage / apparent size"),
        ("v", "tree and map / tree / map"),
        ("r", "scan again"),
        ("Esc", "stop a scan, close a dialog"),
        ("click", "select (tree or map)"),
        ("?", "this help"),
        ("q", "quit"),
    ];
    let w = area.width.saturating_sub(4).min(58);
    let h = (KEYS.len() as u16 + 4).min(area.height);
    let r = Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    );
    let style = Style::default().bg(Color::Indexed(235)).fg(Color::White);
    frame(r, " Keys ", style, buf);
    for (i, (k, what)) in KEYS.iter().enumerate().take(h.saturating_sub(4) as usize) {
        let line = format!("{k:<22}{what}");
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

/// A box with a title, filled with `style`; only the part inside the buffer is drawn.
fn frame(r: Rect, title: &str, style: Style, buf: &mut Buffer) {
    buf.set_style(r, style);
    let seen = r.intersection(buf.area);
    for y in seen.y..seen.bottom() {
        for x in seen.x..seen.right() {
            let edge_x = x == r.x || x + 1 == r.right();
            let edge_y = y == r.y || y + 1 == r.bottom();
            let s = match (edge_x, edge_y, x == r.x, y == r.y) {
                (true, true, true, true) => "┌",
                (true, true, false, true) => "┐",
                (true, true, true, false) => "└",
                (true, true, false, false) => "┘",
                (false, true, _, _) => "─",
                (true, false, _, _) => "│",
                _ => " ",
            };
            buf[(x, y)].set_symbol(s);
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

/// `s` cut to `w` cells, ending with `…` if cut.
fn fit_end(s: &str, w: usize) -> String {
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
        out.push('…');
    }
    out
}

/// `s` cut to `w` cells in the middle (paths keep their start and their name).
fn fit_middle(s: &str, w: usize) -> String {
    if width(s) <= w || w < 5 {
        return fit_end(s, w);
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
    let head = fit_end(s, keep - used + 1);
    let head = head.trim_end_matches('…');
    format!("{head}…{}", tail.into_iter().collect::<String>())
}

/// A bar `w` cells long, `share` (0 to 1) of it filled, in eighths of a cell.
fn bar(share: f64, w: u16) -> String {
    const PART: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];
    let eighths = (share.clamp(0.0, 1.0) * f64::from(w) * 8.0).round() as usize;
    let mut s = "█".repeat(eighths / 8);
    s.push_str(PART[eighths % 8]);
    let filled = eighths.div_ceil(8);
    s.push_str(&" ".repeat((w as usize).saturating_sub(filled)));
    s
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

    #[test]
    fn text_fits_its_cells() {
        assert_eq!(fit_end("abcdef", 4), "abc…");
        assert_eq!(fit_end("abc", 4), "abc");
        assert_eq!(width(&fit_end("檔案名稱很長", 7)), 7);
        let p = fit_middle("/home/me/a/very/long/path/name.txt", 20);
        assert_eq!(width(&p), 20);
        assert!(p.starts_with("/home") && p.ends_with("name.txt"), "{p}");
        assert_eq!(clean("caf\n x"), "caf? x");
        assert_eq!(bar(0.5, 4), "██  ");
        assert_eq!(width(&bar(0.33, 10)), 10);
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
