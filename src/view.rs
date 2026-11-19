// SPDX-License-Identifier: GPL-3.0-only

//! The embeddable view: what is on screen and what keys and clicks do to it. It draws into a
//! ratatui `Buffer` (draw.rs) and takes crossterm events, both only as data: the terminal itself
//! belongs to the host (the CLI in main.rs, or termcode).

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::mem;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;

use crate::guard::{self, Policy};
use crate::style::{Colors, IconSet};
use crate::sys;
use crate::tree::{Node, NodeId, Tree, HARDLINK_DUP, TRASHED};
use crate::{scan, Scan};

/// What the host should do after an event.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    None,
    /// Draw again.
    Redraw,
    Quit,
    /// Open this path with the desktop's default program.
    Open(PathBuf),
    /// Put this path on the clipboard.
    CopyPath(PathBuf),
}

/// How long a new confirmation dialog takes only Cancel (keys typed ahead, a key held down).
pub(crate) const ARM: Duration = Duration::from_millis(500);
/// How long the focus must have been on "Move to Trash" before Enter there moves the item.
pub(crate) const REFOCUS: Duration = Duration::from_millis(150);
/// Two clicks on the same spot within this are a double click.
const DOUBLE: Duration = Duration::from_millis(400);
/// How often a running scan's progress is drawn.
const PROGRESS: Duration = Duration::from_millis(100);
/// How often a running scan is checked for its end, so its tree shows at most this late.
/// (crossterm's wait for input cannot be woken from another thread, so this is a poll; it
/// stops when the scan does.)
const SCAN_CHECK: Duration = Duration::from_millis(15);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Panes {
    Both,
    Tree,
    Map,
}

/// A column the tree can be sorted by (the treemap is always by size, largest first).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Field {
    Name,
    Size,
    Items,
    Modified,
}

/// The tree's order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Sort {
    pub by: Field,
    pub descending: bool,
}

impl Default for Sort {
    fn default() -> Sort {
        Sort {
            by: Field::Size,
            descending: true,
        }
    }
}

impl Sort {
    /// Sorted by `field`: the field it is sorted by already turns round; another starts in its
    /// natural order (names A to Z; sizes, item counts and times largest or newest first).
    pub fn by(self, field: Field) -> Sort {
        match field == self.by {
            true => Sort {
                descending: !self.descending,
                ..self
            },
            false => Sort {
                by: field,
                descending: field != Field::Name,
            },
        }
    }
}

pub struct View {
    pub(crate) policy: Policy,
    pub(crate) screen: Screen,
    /// Apparent sizes (file lengths) rather than disk usage.
    pub(crate) apparent: bool,
    pub(crate) panes: Panes,
    pub(crate) help: bool,
    pub(crate) message: Option<String>,
    /// The message says something was done (drawn with a check mark).
    pub(crate) done: bool,
    /// Where things were drawn last, for the mouse.
    pub(crate) hits: Hits,
    /// The view is drawn here, the size of its area, then copied out: nothing it draws can
    /// land outside the area it was given (a host's other panes stay untouched).
    pub(crate) canvas: Buffer,
    last_click: Option<(Instant, u16, u16)>,
    /// When a running scan's progress was last asked to be drawn, and when the scan started.
    progress_drawn: Instant,
    pub(crate) scan_started: Instant,
    pub(crate) colors: Colors,
    pub(crate) icons: IconSet,
    /// The treemap's letters are by age rather than by type.
    pub(crate) by_age: bool,
    /// The tree's order (kept for the next scan, as `apparent` is).
    sort: Sort,
    /// Where the mouse pointer was last seen, and what was under it in the last drawing.
    pub(crate) pointer: Option<(u16, u16)>,
    pub(crate) hover: Hover,
}

/// What the pointer is over, as far as drawing it differently goes. Styling only: what a click
/// does never depends on it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Hover {
    #[default]
    Nothing,
    Place(usize),
    /// A tree row (its index in `Browse::rows`), a tile (in `Hits::tiles`), a status bar item, a
    /// column heading (in `Hits::heads`).
    Row(usize),
    Tile(usize),
    Item(usize),
    Head(usize),
    Cancel,
    Move,
}

pub(crate) enum Screen {
    Start(Start),
    Scanning(Scan),
    Browse(Box<Browse>),
    /// Only while one screen is being swapped for another.
    Gone,
}

pub(crate) struct Start {
    pub sel: usize,
    pub places: Vec<Place>,
    /// (used, total) of the home filesystem.
    pub disk: Option<(u64, u64)>,
}

pub(crate) struct Place {
    pub name: &'static str,
    pub path: PathBuf,
    /// Its items could be moved to the home trash (same filesystem).
    pub trashable: bool,
    pub exists: bool,
}

pub(crate) struct Browse {
    pub tree: Tree,
    /// The folder shown: the tree's top row and the treemap.
    pub zoom: NodeId,
    pub open: HashSet<NodeId>,
    /// The rows of the tree pane: node and depth below `zoom`.
    pub rows: Vec<(NodeId, u16)>,
    pub cursor: usize,
    /// The first row on screen.
    pub top: usize,
    /// Rows the tree pane showed last (for paging).
    pub page: usize,
    /// Whether the scanned filesystem is the trash's (else nothing here can be trashed).
    pub trash_reaches: bool,
    pub confirm: Option<Confirm>,
    /// Sorted by apparent size rather than disk usage (the view's mode).
    apparent: bool,
    /// The tree's order (the view's).
    pub sort: Sort,
    /// Children sorted by size, largest first, trashed ones left out (the treemap's order).
    sorted: HashMap<NodeId, Vec<NodeId>>,
    /// The same in the tree's order.
    ordered: HashMap<NodeId, Vec<NodeId>>,
}

/// Which handler a key goes to.
enum Mode {
    Start,
    Scanning,
    Confirm,
    Browse,
    Gone,
}

pub(crate) struct Confirm {
    pub id: NodeId,
    pub path: PathBuf,
    pub opened: Instant,
    /// The focus is on "Move to Trash" (it starts on Cancel).
    pub on_move: bool,
    /// When the focus last moved.
    pub focused: Instant,
    /// The armed look has been drawn.
    pub shown_armed: bool,
    /// The last drawing showed the whole path; until then (and while it cannot) the item
    /// cannot be moved.
    pub fits: bool,
}

impl Confirm {
    pub fn armed(&self) -> bool {
        self.opened.elapsed() >= ARM
    }

    /// Whether a click on "Move to Trash" may move the item: armed and the whole path shown.
    /// The button is drawn disabled exactly when this is false (the keyboard's `REFOCUS` wait
    /// is not shown).
    pub fn clickable(&self) -> bool {
        self.armed() && self.fits
    }

    /// Whether Enter on "Move to Trash" may move the item now: armed, the whole path shown,
    /// and the focus there for `REFOCUS` (a Tab and an Enter in one burst, as pasted text
    /// brings them where bracketed paste is missing, never confirm).
    pub fn may_move(&self) -> bool {
        self.on_move && self.armed() && self.fits && self.focused.elapsed() >= REFOCUS
    }
}

#[derive(Default)]
pub(crate) struct Hits {
    /// The tree pane's rows.
    pub rows: Rect,
    pub tiles: Vec<(Rect, Option<NodeId>)>,
    /// The start screen's list.
    pub places: Rect,
    /// The dialog's Cancel and Move buttons.
    pub buttons: Option<(Rect, Rect)>,
    /// The tree's scrollbar column (the wheel scrolls there too; a click does nothing).
    pub scrollbar: Rect,
    /// The status bar's items, and the key a click on one presses.
    pub items: Vec<(Rect, char)>,
    /// The tree's column headings, and what a click on one sorts by.
    pub heads: Vec<(Rect, Field)>,
}

impl View {
    /// A view that scans `path` at once, or starts with the place list if `path` is `None`.
    pub fn new(policy: Policy, path: Option<&Path>) -> View {
        let mut view = View {
            policy,
            screen: Screen::Gone,
            apparent: false,
            panes: Panes::Both,
            help: false,
            message: None,
            done: false,
            hits: Hits::default(),
            canvas: Buffer::empty(Rect::default()),
            colors: Colors::default(),
            icons: IconSet::default(),
            last_click: None,
            progress_drawn: Instant::now(),
            scan_started: Instant::now(),
            by_age: false,
            sort: Sort::default(),
            pointer: None,
            hover: Hover::Nothing,
        };
        match path {
            Some(p) => view.start_scan(p),
            None => view.screen = Screen::Start(view.start()),
        }
        view
    }

    fn start(&self) -> Start {
        let home = self.policy.home().to_path_buf();
        let places = [
            ("Home", home.clone()),
            ("Cache", self.policy.cache_home().to_path_buf()),
            ("Temporary files", PathBuf::from("/tmp")),
            ("Trash", self.policy.trash_dir()),
        ];
        let places = places
            .into_iter()
            .map(|(name, path)| {
                let st = sys::stat_path(&path);
                Place {
                    name,
                    trashable: st.as_ref().is_ok_and(|s| self.policy.trash_reaches(s.dev)),
                    exists: st.is_ok(),
                    path,
                }
            })
            .collect();
        Start {
            sel: 0,
            places,
            disk: sys::fs_usage(&home).ok(),
        }
    }

    fn start_scan(&mut self, path: &Path) {
        match scan(path, self.policy.euid) {
            Ok(s) => {
                self.screen = Screen::Scanning(s);
                self.progress_drawn = Instant::now();
                self.scan_started = self.progress_drawn;
            }
            Err(e) => {
                self.message = Some(format!("Cannot scan {}: {e}", path.display()));
                self.screen = Screen::Start(self.start());
            }
        }
    }

    /// Moves a finished scan on to the tree, and arms a waiting dialog. Returns whether the
    /// screen changed (a running scan's progress does every `PROGRESS`).
    pub fn tick(&mut self) -> bool {
        if matches!(&self.screen, Screen::Scanning(s) if s.is_finished()) {
            if let Screen::Scanning(s) = mem::replace(&mut self.screen, Screen::Gone) {
                self.screen = match s.join() {
                    Some(tree) => Screen::Browse(Box::new(self.browse(tree))),
                    None => Screen::Start(self.start()),
                };
            }
            return true;
        }
        match &mut self.screen {
            Screen::Scanning(_) if self.progress_drawn.elapsed() >= PROGRESS => {
                self.progress_drawn = Instant::now();
                true
            }
            Screen::Browse(b) => match &mut b.confirm {
                Some(c) if !c.shown_armed && c.armed() => {
                    c.shown_armed = true;
                    true
                }
                _ => false,
            },
            _ => false,
        }
    }

    /// When `tick` next has something to do: `None` means only an event can change anything
    /// (the host may block until one comes).
    pub fn next_wake(&self) -> Option<Duration> {
        match &self.screen {
            Screen::Scanning(_) => Some(SCAN_CHECK),
            Screen::Browse(b) => b
                .confirm
                .as_ref()
                .filter(|c| !c.shown_armed)
                .map(|c| ARM.saturating_sub(c.opened.elapsed())),
            _ => None,
        }
    }

    fn browse(&self, mut tree: Tree) -> Browse {
        self.policy.mark(&mut tree);
        let trash_reaches = self.policy.trash_reaches(tree.dev);
        let mut b = Browse {
            tree,
            zoom: 0,
            open: HashSet::from([0]),
            rows: Vec::new(),
            cursor: 0,
            top: 0,
            page: 10,
            trash_reaches,
            confirm: None,
            apparent: self.apparent,
            sort: self.sort,
            sorted: HashMap::new(),
            ordered: HashMap::new(),
        };
        b.rebuild(None);
        b.cursor = usize::from(b.rows.len() > 1);
        b
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Outcome {
        if key.kind == KeyEventKind::Release {
            return Outcome::None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            return Outcome::Quit;
        }
        if self.help {
            self.help = false;
            return Outcome::Redraw;
        }
        let had_message = self.message.take().is_some();
        self.done = false;
        let mode = match &self.screen {
            Screen::Start(_) => Mode::Start,
            Screen::Scanning(_) => Mode::Scanning,
            Screen::Browse(b) if b.confirm.is_some() => Mode::Confirm,
            Screen::Browse(_) => Mode::Browse,
            Screen::Gone => Mode::Gone,
        };
        let out = match mode {
            Mode::Start => self.start_key(key),
            Mode::Scanning => match (&self.screen, key.code) {
                (Screen::Scanning(s), KeyCode::Esc) => {
                    s.cancel();
                    Outcome::Redraw
                }
                (_, KeyCode::Char('q')) => Outcome::Quit,
                _ => Outcome::None,
            },
            Mode::Confirm => self.confirm_key(key),
            Mode::Browse => self.browse_key(key),
            Mode::Gone => Outcome::None,
        };
        match out {
            Outcome::None if had_message => Outcome::Redraw,
            out => out,
        }
    }

    fn start_key(&mut self, key: KeyEvent) -> Outcome {
        let Screen::Start(s) = &mut self.screen else {
            return Outcome::None;
        };
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => s.sel = s.sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => s.sel = (s.sel + 1).min(s.places.len() - 1),
            KeyCode::Enter => return self.open_place(),
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('q') | KeyCode::Esc => return Outcome::Quit,
            _ => return Outcome::None,
        }
        Outcome::Redraw
    }

    fn open_place(&mut self) -> Outcome {
        let Screen::Start(s) = &self.screen else {
            return Outcome::None;
        };
        let place = &s.places[s.sel];
        if !place.exists {
            self.message = Some(format!("{} does not exist", place.path.display()));
        } else {
            let path = place.path.clone();
            self.start_scan(&path);
        }
        Outcome::Redraw
    }

    fn browse_key(&mut self, key: KeyEvent) -> Outcome {
        let read_only = self.policy.read_only().is_some();
        let Screen::Browse(b) = &mut self.screen else {
            return Outcome::None;
        };
        let sel = b.rows[b.cursor].0;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => b.cursor = b.cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => b.cursor = (b.cursor + 1).min(b.rows.len() - 1),
            KeyCode::PageUp => b.cursor = b.cursor.saturating_sub(b.page),
            KeyCode::PageDown => b.cursor = (b.cursor + b.page).min(b.rows.len() - 1),
            KeyCode::Home => b.cursor = 0,
            KeyCode::End => b.cursor = b.rows.len() - 1,
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Char('+') => {
                if b.tree.node(sel).is_dir() && b.open.insert(sel) {
                    b.rebuild(Some(sel));
                } else if b.open.contains(&sel) && b.cursor + 1 < b.rows.len() {
                    b.cursor += 1;
                }
            }
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Char('-') => {
                if sel != b.zoom && b.open.remove(&sel) {
                    b.rebuild(Some(sel));
                } else if let Some(p) = b.tree.parent(sel).filter(|_| sel != b.zoom) {
                    b.select(p);
                }
            }
            KeyCode::Enter => b.zoom_in(sel),
            KeyCode::Backspace => b.zoom_out(),
            KeyCode::Char('v') => {
                self.panes = match self.panes {
                    Panes::Both => Panes::Tree,
                    Panes::Tree => Panes::Map,
                    Panes::Map => Panes::Both,
                }
            }
            KeyCode::Char('a') => {
                self.apparent = !self.apparent;
                b.apparent = self.apparent;
                b.sorted.clear();
                b.ordered.clear();
                b.rebuild(Some(sel));
            }
            KeyCode::Char('t') => self.by_age = !self.by_age,
            // The next column in its natural order, or the same one turned round.
            KeyCode::Char('s') | KeyCode::Char('S') => {
                let field = match (key.code, self.sort.by) {
                    (KeyCode::Char('S'), by) => by,
                    (_, Field::Name) => Field::Size,
                    (_, Field::Size) => Field::Items,
                    (_, Field::Items) => Field::Modified,
                    (_, Field::Modified) => Field::Name,
                };
                self.sort = self.sort.by(field);
                b.resort(self.sort, sel);
            }
            KeyCode::Char('r') => {
                let root = b.tree.root_path().to_path_buf();
                self.start_scan(&root);
            }
            KeyCode::Char('o') => return Outcome::Open(b.tree.path(sel)),
            KeyCode::Char('c') => {
                self.message = Some(format!("Copied {}", b.tree.path(sel).display()));
                return Outcome::CopyPath(b.tree.path(sel));
            }
            KeyCode::Delete if !read_only => self.ask_trash(),
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('q') => return Outcome::Quit,
            KeyCode::Esc => {}
            _ => return Outcome::None,
        }
        Outcome::Redraw
    }

    /// Opens the confirmation dialog for the selected item, or says why it cannot be trashed.
    fn ask_trash(&mut self) {
        let Screen::Browse(b) = &mut self.screen else {
            return;
        };
        let id = b.rows[b.cursor].0;
        // ponytail: `check` here and `trash` on confirm each walk the whole subtree on the
        // thread that draws: 10-20 ms for 10 000 entries warm, seconds for a huge folder read
        // cold. When that shows, run them on a worker thread (`trash` takes `&Tree` for it).
        if let Err(r) = guard::check(&b.tree, id, &self.policy) {
            self.message = Some(format!("Cannot move to the trash: {r}"));
            return;
        }
        let now = Instant::now();
        b.confirm = Some(Confirm {
            id,
            path: b.tree.path(id),
            opened: now,
            on_move: false,
            focused: now,
            shown_armed: false,
            fits: false,
        });
    }

    fn confirm_key(&mut self, key: KeyEvent) -> Outcome {
        let Screen::Browse(b) = &mut self.screen else {
            return Outcome::None;
        };
        let Some(c) = &mut b.confirm else {
            return Outcome::None;
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('n') => b.confirm = None,
            KeyCode::Enter if !c.on_move => b.confirm = None,
            KeyCode::Enter if c.may_move() => self.do_trash(),
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Left | KeyCode::Right if c.armed() => {
                c.on_move = !c.on_move;
                c.focused = Instant::now();
            }
            _ => return Outcome::None,
        }
        Outcome::Redraw
    }

    /// Moves the item in the dialog to the trash (the guard checks it all again).
    fn do_trash(&mut self) {
        let Screen::Browse(b) = &mut self.screen else {
            return;
        };
        let Some(c) = b.confirm.take() else {
            return;
        };
        let n = *b.tree.node(c.id);
        let size = crate::fmt_size(if self.apparent { n.apparent } else { n.alloc });
        let what = if n.is_dir() {
            format!("{} items, {size}", n.items)
        } else {
            size
        };
        let name = crate::draw::shown(b.tree.name(c.id));
        // Shown until the next key or click (no timer); that key or click also does its own
        // work, as with every message.
        self.message = Some(match guard::trash(&b.tree, c.id, &self.policy) {
            Ok(_) => {
                self.done = true;
                b.tree.mark_trashed(c.id);
                b.sorted.clear();
                b.ordered.clear();
                if c.id == b.zoom {
                    b.zoom = b.tree.parent(c.id).unwrap_or(0);
                }
                let at = b.cursor;
                b.rebuild(None);
                b.cursor = at.min(b.rows.len() - 1);
                format!("Moved to the trash: {name}, {what} - freed when the trash is emptied")
            }
            Err(r) => format!("Not moved: {r}"),
        });
    }

    pub fn handle_mouse(&mut self, m: MouseEvent) -> Outcome {
        let (x, y) = (m.column, m.row);
        let inside = |r: Rect| x >= r.x && x < r.right() && y >= r.y && y < r.bottom();
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {}
            // Drawn again only when what is under the pointer changes.
            MouseEventKind::Moved => {
                self.pointer = Some((x, y));
                let h = self.hover_at(x, y);
                return match mem::replace(&mut self.hover, h) != h {
                    true => Outcome::Redraw,
                    false => Outcome::None,
                };
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let up = m.kind == MouseEventKind::ScrollUp;
                let tree = inside(self.hits.rows) || inside(self.hits.scrollbar);
                if let Screen::Browse(b) = &mut self.screen {
                    if b.confirm.is_none() && tree {
                        b.cursor = match up {
                            true => b.cursor.saturating_sub(3),
                            false => (b.cursor + 3).min(b.rows.len() - 1),
                        };
                        return Outcome::Redraw;
                    }
                }
                return Outcome::None;
            }
            _ => return Outcome::None,
        }
        let now = Instant::now();
        let double = self
            .last_click
            .is_some_and(|(t, cx, cy)| now - t < DOUBLE && (cx, cy) == (x, y));
        // A double click is used up: a third quick click starts over as a single one.
        self.last_click = if double { None } else { Some((now, x, y)) };
        if self.help {
            self.help = false;
            return Outcome::Redraw;
        }
        self.message = None;
        self.done = false;
        let item = self.hits.items.iter().find(|i| inside(i.0)).map(|i| i.1);
        let browsing = matches!(&self.screen, Screen::Browse(b) if b.confirm.is_none());
        if let Some(k) = item.filter(|_| browsing) {
            // A status bar item does what its key does.
            self.browse_key(KeyEvent::new(KeyCode::Char(k), KeyModifiers::NONE));
            return Outcome::Redraw;
        }
        // A column heading sorts the tree by it.
        let head = self.hits.heads.iter().find(|h| inside(h.0)).map(|h| h.1);
        match &mut self.screen {
            Screen::Start(s) if inside(self.hits.places) => {
                let row = usize::from(y - self.hits.places.y);
                if row < s.places.len() {
                    s.sel = row;
                    if double {
                        return self.open_place();
                    }
                }
            }
            Screen::Browse(b) if b.confirm.is_some() => {
                let Some((cancel, moving)) = self.hits.buttons else {
                    return Outcome::None;
                };
                // A click is aimed: no wait for the focus, but armed and the whole path shown.
                let ok = b.confirm.as_ref().is_some_and(Confirm::clickable);
                if inside(cancel) {
                    b.confirm = None;
                } else if inside(moving) && ok {
                    self.do_trash();
                } else {
                    return Outcome::None;
                }
            }
            Screen::Browse(b) if head.is_some() => {
                let sel = b.rows[b.cursor].0;
                self.sort = head.map_or(self.sort, |f| self.sort.by(f));
                b.resort(self.sort, sel);
            }
            Screen::Browse(b) if inside(self.hits.rows) => {
                let row = b.top + usize::from(y - self.hits.rows.y);
                if row < b.rows.len() {
                    b.cursor = row;
                    if double {
                        b.toggle(b.rows[row].0);
                    }
                }
            }
            Screen::Browse(b) => {
                let hit = self
                    .hits
                    .tiles
                    .iter()
                    .find(|(r, _)| inside(*r))
                    .map(|t| t.1);
                match hit {
                    Some(Some(id)) => {
                        b.select(id);
                        if double {
                            b.zoom_in(id);
                        }
                    }
                    Some(None) => {
                        self.message = Some("Small items, together: see them in the tree".into())
                    }
                    None => return Outcome::None,
                }
            }
            _ => return Outcome::None,
        }
        Outcome::Redraw
    }

    /// What is under (x, y) in the last drawing, for hover: the same places, in the same order,
    /// as a click looks for them in `handle_mouse`.
    pub(crate) fn hover_at(&self, x: u16, y: u16) -> Hover {
        let inside = |r: Rect| x >= r.x && x < r.right() && y >= r.y && y < r.bottom();
        let h = &self.hits;
        if self.help {
            return Hover::Nothing;
        }
        match &self.screen {
            Screen::Start(s) if inside(h.places) => match usize::from(y - h.places.y) {
                row if row < s.places.len() => Hover::Place(row),
                _ => Hover::Nothing,
            },
            Screen::Browse(b) if b.confirm.is_some() => match h.buttons {
                Some((cancel, _)) if inside(cancel) => Hover::Cancel,
                Some((_, moving)) if inside(moving) => Hover::Move,
                _ => Hover::Nothing,
            },
            Screen::Browse(_) if h.items.iter().any(|i| inside(i.0)) => {
                Hover::Item(h.items.iter().position(|i| inside(i.0)).unwrap_or(0))
            }
            Screen::Browse(_) if h.heads.iter().any(|i| inside(i.0)) => {
                Hover::Head(h.heads.iter().position(|i| inside(i.0)).unwrap_or(0))
            }
            Screen::Browse(b) if inside(h.rows) => match b.top + usize::from(y - h.rows.y) {
                row if row < b.rows.len() => Hover::Row(row),
                _ => Hover::Nothing,
            },
            Screen::Browse(_) => h
                .tiles
                .iter()
                .position(|t| inside(t.0))
                .map_or(Hover::Nothing, Hover::Tile),
            _ => Hover::Nothing,
        }
    }

    /// The colours to draw with (the default is termcode's; a host passes its own, already
    /// fitted to the terminal, or fits the frame afterwards as termcode does).
    pub fn set_colors(&mut self, colors: Colors) {
        self.colors = colors;
    }

    /// The glyphs to draw with (default: Unicode, which any font has).
    pub fn set_icons(&mut self, icons: IconSet) {
        self.icons = icons;
    }

    /// Draws the view into `buf`, over `area`.
    pub fn draw(&mut self, area: Rect, buf: &mut Buffer) {
        crate::draw::draw(self, area, buf);
    }
}

/// What a node adds to its folder's total in this mode (a repeated hard link adds nothing).
pub(crate) fn counted(n: &Node, apparent: bool) -> u64 {
    match (n.flags & HARDLINK_DUP != 0, apparent) {
        (true, _) => 0,
        (false, true) => n.apparent,
        (false, false) => n.alloc,
    }
}

impl Browse {
    /// The children of `id`, largest first, without trashed ones.
    pub fn kids(&mut self, id: NodeId) -> &[NodeId] {
        let (tree, apparent) = (&self.tree, self.apparent);
        self.sorted.entry(id).or_insert_with(|| {
            let mut kids: Vec<NodeId> = tree
                .children(id)
                .filter(|&k| tree.node(k).flags & TRASHED == 0)
                .collect();
            kids.sort_by_key(|&k| std::cmp::Reverse(counted(tree.node(k), apparent)));
            kids
        })
    }

    /// Lays the rows out again from `zoom` down through the open folders, keeping the cursor
    /// on `keep` if it is still shown.
    fn rebuild(&mut self, keep: Option<NodeId>) {
        let mut rows = Vec::new();
        let mut stack = vec![(self.zoom, 0u16)];
        while let Some((id, depth)) = stack.pop() {
            rows.push((id, depth));
            if self.open.contains(&id) || id == self.zoom {
                let kids = self.ordered(id);
                stack.extend(kids.into_iter().rev().map(|k| (k, depth + 1)));
            }
        }
        self.rows = rows;
        // On `keep`, or, if a folder above it was closed, on the nearest one still shown.
        let mut keep = keep;
        while let Some(k) = keep {
            if let Some(i) = self.rows.iter().position(|r| r.0 == k) {
                self.cursor = i;
                break;
            }
            keep = self.tree.parent(k);
        }
        self.cursor = self.cursor.min(self.rows.len() - 1);
    }

    /// Opens or closes the folder `id` in place. The shown folder is always open, and a file
    /// has nothing to open: both stay as they are.
    fn toggle(&mut self, id: NodeId) {
        if id == self.zoom || !self.tree.node(id).is_dir() {
            return;
        }
        if !self.open.remove(&id) {
            self.open.insert(id);
        }
        self.rebuild(Some(id));
    }

    /// Puts the cursor on `id`, opening the folders above it as needed.
    pub fn select(&mut self, id: NodeId) {
        let mut up = self.tree.parent(id);
        while let Some(p) = up {
            self.open.insert(p);
            if p == self.zoom {
                break;
            }
            up = self.tree.parent(p);
        }
        if !self.rows.iter().any(|r| r.0 == id) {
            self.rebuild(Some(id));
        }
        if let Some(i) = self.rows.iter().position(|r| r.0 == id) {
            self.cursor = i;
        }
    }

    fn zoom_in(&mut self, id: NodeId) {
        if id == self.zoom || !self.tree.node(id).is_dir() {
            return;
        }
        self.zoom = id;
        self.open.insert(id);
        self.rebuild(None);
        self.cursor = usize::from(self.rows.len() > 1);
        self.top = 0;
    }

    fn zoom_out(&mut self) {
        let Some(up) = self.tree.parent(self.zoom) else {
            return;
        };
        let was = self.zoom;
        self.zoom = up;
        self.rebuild(Some(was));
    }

    /// The treemap's folder and its selected tile: the folder that holds the selected row, and
    /// that row's item; on the shown folder's own row, that folder and no tile.
    pub fn map_of(&self) -> (NodeId, Option<NodeId>) {
        let sel = self.rows[self.cursor].0;
        match self.tree.parent(sel).filter(|_| sel != self.zoom) {
            Some(p) => (p, Some(sel)),
            None => (self.zoom, None),
        }
    }

    /// The children of `id` in the tree's order: folders first, then the rest, each in the
    /// sorted column's order; `kids` sorted again, stably, so items equal there stay largest
    /// first.
    fn ordered(&mut self, id: NodeId) -> Vec<NodeId> {
        if let Some(kids) = self.ordered.get(&id) {
            return kids.clone();
        }
        let mut kids = self.kids(id).to_vec();
        let (tree, apparent, sort) = (&self.tree, self.apparent, self.sort);
        kids.sort_by(|&a, &b| {
            let (x, y) = (tree.node(a), tree.node(b));
            let order = match sort.by {
                Field::Name => by_name(tree.name(a).as_bytes(), tree.name(b).as_bytes()),
                Field::Size => counted(x, apparent).cmp(&counted(y, apparent)),
                Field::Items => x.items.cmp(&y.items), // a file has none
                Field::Modified => x.mtime.cmp(&y.mtime),
            };
            let order = if sort.descending {
                order.reverse()
            } else {
                order
            };
            // Folders first, whichever way round the rest goes.
            y.is_dir().cmp(&x.is_dir()).then(order)
        });
        self.ordered.insert(id, kids.clone());
        kids
    }

    /// The tree in `sort`'s order, the selection still on `keep`.
    fn resort(&mut self, sort: Sort, keep: NodeId) {
        self.sort = sort;
        self.ordered.clear();
        self.rebuild(Some(keep));
    }
}

/// Names A to Z, capitals and small letters together (ASCII only), then byte by byte.
fn by_name(a: &[u8], b: &[u8]) -> Ordering {
    let (x, y) = (a.iter(), b.iter());
    x.map(u8::to_ascii_lowercase)
        .cmp(y.map(u8::to_ascii_lowercase))
        .then_with(|| a.cmp(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TempDir;
    use std::fs;
    use std::thread::sleep;

    fn me() -> u32 {
        unsafe { libc::geteuid() }
    }

    /// A temp home with a cache (a big folder, a small file, a hard-linked file) and settings.
    fn home(tag: &str) -> (TempDir, PathBuf) {
        let t = TempDir::new(tag);
        let home = fs::canonicalize(&t.0).unwrap().join("home");
        for d in [
            ".local/share",
            ".config/app",
            ".cache/big/inner",
            "Downloads",
        ] {
            fs::create_dir_all(home.join(d)).unwrap();
        }
        for i in 0..20 {
            fs::write(
                home.join(format!(".cache/big/inner/f{i}")),
                vec![1u8; 20_000],
            )
            .unwrap();
        }
        fs::write(home.join(".cache/small"), vec![2u8; 3000]).unwrap();
        fs::write(home.join(".cache/linked"), vec![3u8; 9000]).unwrap();
        fs::hard_link(home.join(".cache/linked"), home.join("Downloads/linked")).unwrap();
        fs::write(home.join(".config/app/rc"), b"x").unwrap();
        (t, home)
    }

    fn policy(home: &Path, read_only: Option<String>) -> Policy {
        let (data, config) = (home.join(".local/share"), home.join(".config"));
        Policy::new(
            me(),
            None,
            &[home.to_path_buf()],
            &data,
            &config,
            None,
            read_only,
        )
    }

    /// A view of `path`, its scan done.
    fn ready(policy: Policy, path: &Path) -> View {
        let mut v = View::new(policy, Some(path));
        for _ in 0..5000 {
            v.tick();
            if matches!(v.screen, Screen::Browse(_)) {
                return v;
            }
            sleep(Duration::from_millis(1));
        }
        panic!("the scan did not finish");
    }

    fn text(v: &mut View, w: u16, h: u16) -> Vec<String> {
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        v.draw(area, &mut buf);
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect())
            .collect()
    }

    fn has(lines: &[String], s: &str) -> bool {
        lines.iter().any(|l| l.contains(s))
    }

    fn key(v: &mut View, code: KeyCode) -> Outcome {
        v.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn click(v: &mut View, x: u16, y: u16) -> Outcome {
        v.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn browse(v: &mut View) -> &mut Browse {
        match &mut v.screen {
            Screen::Browse(b) => b,
            _ => panic!("not browsing"),
        }
    }

    fn selected_name(v: &mut View) -> String {
        let b = browse(v);
        let id = b.rows[b.cursor].0;
        b.tree.name(id).to_string_lossy().into_owned()
    }

    /// Puts the cursor on the row named `name`.
    fn select(v: &mut View, name: &str) {
        let b = browse(v);
        let i = b
            .rows
            .iter()
            .position(|r| b.tree.name(r.0) == name)
            .unwrap();
        b.cursor = i;
    }

    #[test]
    fn the_tree_and_the_map_show_the_folder() {
        let (_t, home) = home("view-show");
        let mut v = ready(policy(&home, None), &home.join(".cache"));
        assert_eq!(
            selected_name(&mut v),
            "big",
            "the largest item starts selected"
        );
        let lines = text(&mut v, 120, 40);
        assert!(has(&lines, "Name") && has(&lines, "Size") && has(&lines, "Modified"));
        assert!(has(&lines, "▸ big/") && has(&lines, "small"));
        let map: Vec<String> = lines.iter().map(|l| l.chars().skip(62).collect()).collect();
        assert!(has(&map, "big/"), "the treemap names the folder: {map:?}");
        assert!(has(&lines, "Del trash"));
        let small = text(&mut v, 80, 24);
        assert!(has(&small, "Name") && has(&small, "big/"));
        for (w, h) in [(30, 8), (20, 5), (24, 6)] {
            text(&mut v, w, h); // no panic in a small window
        }
    }

    #[test]
    fn rule7_the_dialog_says_what_and_enter_alone_cancels() {
        let (_t, home) = home("view-dialog");
        let mut v = ready(policy(&home, None), &home.join(".cache"));
        key(&mut v, KeyCode::Delete);
        let lines = text(&mut v, 120, 40);
        let path = home.join(".cache/big").display().to_string();
        assert!(has(&lines, &path), "the full path");
        assert!(has(&lines, "items") && has(&lines, "only when the trash is emptied"));
        assert!(has(&lines, "[ Cancel ]") && has(&lines, "[ Move to Trash ]"));
        key(&mut v, KeyCode::Enter);
        assert!(
            browse(&mut v).confirm.is_none(),
            "Enter on Cancel closes it"
        );
        assert!(home.join(".cache/big").exists());
    }

    #[test]
    fn rule7_only_cancel_for_half_a_second_then_tab_enter_moves_it() {
        let (_t, home) = home("view-arm");
        let mut v = ready(policy(&home, None), &home.join(".cache"));
        let before = browse(&mut v).tree.node(0).alloc;
        key(&mut v, KeyCode::Delete);
        assert!(
            v.next_wake().is_some_and(|d| d <= ARM),
            "wakes to arm the dialog"
        );
        key(&mut v, KeyCode::Tab); // too soon: ignored
        key(&mut v, KeyCode::Enter); // still on Cancel
        assert!(browse(&mut v).confirm.is_none() && home.join(".cache/big").exists());
        key(&mut v, KeyCode::Delete);
        age(&mut v);
        assert!(v.tick(), "draws the armed dialog once");
        assert_eq!(v.next_wake(), None);
        key(&mut v, KeyCode::Tab);
        browse(&mut v).confirm.as_mut().unwrap().focused -= REFOCUS;
        key(&mut v, KeyCode::Enter);
        let trash = home.join(".local/share/Trash");
        assert!(!home.join(".cache/big").exists());
        assert!(trash.join("files/big/inner/f19").exists());
        assert!(trash.join("info/big.trashinfo").exists());
        let lines = text(&mut v, 120, 40);
        assert!(has(&lines, "Moved to the trash"));
        assert!(!has(&lines, "big/"), "gone from the tree and the map");
        assert!(
            browse(&mut v).tree.node(0).alloc < before,
            "sizes updated without a rescan"
        );
    }

    /// Ages the open dialog past every delay, and draws it (its whole path is then shown).
    fn age(v: &mut View) {
        let c = browse(v).confirm.as_mut().unwrap();
        c.opened -= Duration::from_secs(1);
        c.focused -= Duration::from_secs(1);
        text(v, 120, 40);
    }

    #[test]
    fn rule7_a_tab_and_an_enter_in_one_burst_never_confirm() {
        // What a paste brings where the terminal has no bracketed paste: a<Tab>b<Enter>.
        let (_t, home) = home("view-burst");
        let mut v = ready(policy(&home, None), &home.join(".cache"));
        key(&mut v, KeyCode::Delete);
        age(&mut v);
        for code in [
            KeyCode::Char('a'),
            KeyCode::Tab,
            KeyCode::Char('b'),
            KeyCode::Enter,
        ] {
            key(&mut v, code);
        }
        let c = browse(&mut v).confirm.as_mut().unwrap();
        assert!(c.on_move, "the Tab moved the focus");
        assert!(
            home.join(".cache/big").exists(),
            "the Enter right after it did nothing"
        );
        c.focused -= REFOCUS;
        key(&mut v, KeyCode::Enter);
        assert!(
            !home.join(".cache/big").exists(),
            "an Enter a moment later does"
        );
    }

    #[test]
    fn rule7_the_dialog_moves_its_own_item() {
        let (_t, home) = home("view-own");
        let mut v = ready(policy(&home, None), &home.join(".cache"));
        key(&mut v, KeyCode::Delete); // on big
        key(&mut v, KeyCode::Down); // the dialog has the keys: the selection stays
        assert_eq!(selected_name(&mut v), "big");
        select(&mut v, "small"); // and if the selection moved anyway
        age(&mut v);
        key(&mut v, KeyCode::Tab);
        browse(&mut v).confirm.as_mut().unwrap().focused -= REFOCUS;
        key(&mut v, KeyCode::Enter);
        assert!(!home.join(".cache/big").exists());
        assert!(home.join(".cache/small").exists());
    }

    #[test]
    fn rule7_the_whole_path_shows_escaped_or_nothing_moves() {
        use std::os::unix::ffi::OsStrExt;
        let (_t, home) = home("view-path");
        // A newline, a right-to-left override (U+202E), a byte that is not UTF-8.
        let name = std::ffi::OsStr::from_bytes(b"odd\n\xe2\x80\xaegpj.exe\xff");
        let odd = home.join(".cache").join(name);
        fs::write(&odd, b"x").unwrap();
        let mut v = ready(policy(&home, None), &home.join(".cache"));
        let id = browse(&mut v).tree.find(&odd).unwrap();
        browse(&mut v).select(id);
        key(&mut v, KeyCode::Delete);
        let shown = crate::draw::shown(odd.as_os_str());
        assert!(shown.ends_with("/odd\\n\\u{202e}gpj.exe\\xff"), "{shown}");
        let lines = text(&mut v, 60, 20);
        let wrapped = crate::draw::wrap(&shown, 54);
        assert!(wrapped.len() > 1, "a path longer than the box wraps");
        for part in &wrapped {
            assert!(has(&lines, part), "{part:?} not shown: {lines:#?}");
        }
        assert!(browse(&mut v).confirm.as_ref().unwrap().fits);
        // Too narrow for the whole path: said so, and Move does nothing.
        let lines = text(&mut v, 30, 12);
        assert!(has(&lines, "too small"));
        let c = browse(&mut v).confirm.as_mut().unwrap();
        (c.opened, c.on_move) = (c.opened - Duration::from_secs(1), true);
        c.focused -= Duration::from_secs(1);
        key(&mut v, KeyCode::Enter);
        assert!(odd.exists());
    }

    fn fits(v: &mut View) -> bool {
        browse(v).confirm.as_ref().unwrap().fits
    }

    /// A file in the cache whose path takes several lines in a 60-column dialog.
    fn long_item(home: &Path) -> PathBuf {
        let dir = home.join(".cache/a-rather-long-folder-name-that-wraps/and-another-long-one");
        fs::create_dir_all(&dir).unwrap();
        let f = dir.join("with-a-long-file-name-too.bin");
        fs::write(&f, b"x").unwrap();
        f
    }

    fn ask_for(v: &mut View, path: &Path) {
        let id = browse(v).tree.find(path).unwrap();
        browse(v).select(id);
        key(v, KeyCode::Delete);
        assert!(browse(v).confirm.is_some());
    }

    #[test]
    fn rule7_the_path_fits_only_with_a_row_for_each_of_its_lines() {
        let (_t, home) = home("view-fits-rows");
        let f = long_item(&home);
        let mut v = ready(policy(&home, None), &home.join(".cache"));
        ask_for(&mut v, &f);
        let lines = crate::draw::wrap(&crate::draw::shown(f.as_os_str()), 54).len() as u16;
        assert!(lines >= 3, "{lines}");
        let need = 7 + lines; // the box around the path, its size, the trash line, the buttons
        text(&mut v, 60, need);
        assert!(fits(&mut v), "a row for every line");
        text(&mut v, 60, need - 1);
        assert!(!fits(&mut v), "one row short");
    }

    #[test]
    fn rule7_a_click_moves_nothing_while_the_path_does_not_fit() {
        let (_t, home) = home("view-fits-click");
        let f = long_item(&home);
        let mut v = ready(policy(&home, None), &home.join(".cache"));
        ask_for(&mut v, &f);
        browse(&mut v).confirm.as_mut().unwrap().opened -= Duration::from_secs(1);
        text(&mut v, 60, 8); // room for the buttons, not for the path
        assert!(!fits(&mut v));
        let (_, moving) = v.hits.buttons.unwrap();
        click(&mut v, moving.x + 2, moving.y);
        assert!(f.exists() && browse(&mut v).confirm.is_some());
    }

    #[test]
    fn rule7_a_window_too_small_to_draw_takes_back_the_move() {
        let (_t, home) = home("view-fits-shrink");
        let mut v = ready(policy(&home, None), &home.join(".cache"));
        key(&mut v, KeyCode::Delete);
        age(&mut v); // drawn at 120x40: the path fits
        assert!(fits(&mut v));
        text(&mut v, 20, 5); // too small to draw any of it
        key(&mut v, KeyCode::Tab);
        browse(&mut v).confirm.as_mut().unwrap().focused -= REFOCUS;
        key(&mut v, KeyCode::Enter);
        assert!(home.join(".cache/big").exists());
    }

    #[test]
    fn a_running_scan_is_checked_often_and_nothing_wakes_after_it() {
        let (_t, home) = home("view-wake");
        let v = View::new(policy(&home, None), Some(&home.join(".cache")));
        assert_eq!(v.next_wake(), Some(SCAN_CHECK));
        assert!(SCAN_CHECK <= Duration::from_millis(20));
        let v = ready(policy(&home, None), &home.join(".cache"));
        assert_eq!(v.next_wake(), None);
    }

    #[test]
    fn every_screen_draws_at_every_size_and_only_in_its_area() {
        let (_t, home) = home("view-sizes");
        let cache = home.join(".cache");
        let mut dialog = ready(policy(&home, None), &cache);
        key(&mut dialog, KeyCode::Delete);
        let mut help = ready(policy(&home, None), &cache);
        key(&mut help, KeyCode::Char('?'));
        let mut message = ready(policy(&home, None), &home);
        select(&mut message, ".config");
        key(&mut message, KeyCode::Delete);
        assert!(message.message.is_some());
        let mut views = [
            View::new(policy(&home, None), None),
            View::new(policy(&home, None), Some(&cache)),
            ready(policy(&home, None), &cache),
            dialog,
            help,
            message,
        ];
        for v in &mut views {
            for w in 0..=60 {
                for h in 0..=12 {
                    let area = Rect::new(0, 0, w, h);
                    v.draw(area, &mut Buffer::empty(area));
                }
            }
            // Inside a host's larger buffer: nothing outside its area changes.
            for area in [
                Rect::new(10, 5, 30, 8),
                Rect::new(0, 0, 7, 3),
                Rect::new(70, 25, 20, 10),
            ] {
                let all = Rect::new(0, 0, 80, 30);
                let mut buf = Buffer::empty(all);
                for c in buf.content.iter_mut() {
                    c.set_symbol("X");
                }
                v.draw(area, &mut buf);
                let mine = area.intersection(all);
                for y in 0..30 {
                    for x in 0..80 {
                        let inside =
                            x >= mine.x && x < mine.right() && y >= mine.y && y < mine.bottom();
                        assert!(
                            inside || buf[(x, y)].symbol() == "X",
                            "({x}, {y}) for {area:?}"
                        );
                    }
                }
            }
        }
    }

    /// One view on each screen: start, scanning, the tree, the dialog, the keys, a refusal; and
    /// what Round B added: the map by age, a tree long enough for its scrollbar, the message
    /// after a trash, and the pointer over a row, a tile, a status item, a button.
    fn every_screen(home: &Path) -> Vec<View> {
        let cache = home.join(".cache");
        let mut dialog = ready(policy(home, None), &cache);
        key(&mut dialog, KeyCode::Delete);
        let mut help = ready(policy(home, None), &cache);
        key(&mut help, KeyCode::Char('?'));
        let mut message = ready(policy(home, None), home);
        select(&mut message, ".config");
        key(&mut message, KeyCode::Delete);
        let mut by_age = ready(policy(home, None), &cache);
        key(&mut by_age, KeyCode::Char('t'));
        let mut long = ready(policy(home, None), &cache);
        let f0 = browse(&mut long)
            .tree
            .find(&cache.join("big/inner/f0"))
            .unwrap();
        browse(&mut long).select(f0);
        fs::write(cache.join("done"), b"x").unwrap(); // each call trashes its own
        let mut trashed = ready(policy(home, None), &cache);
        select(&mut trashed, "done");
        key(&mut trashed, KeyCode::Delete);
        age(&mut trashed);
        let (_, moving) = trashed.hits.buttons.unwrap();
        trashed.last_click = None;
        click(&mut trashed, moving.x + 2, moving.y);
        assert!(trashed
            .message
            .as_ref()
            .unwrap()
            .contains("Moved to the trash"));
        let mut views = vec![
            View::new(policy(home, None), None),
            View::new(policy(home, None), Some(&cache)),
            ready(policy(home, None), &cache),
            dialog,
            help,
            message,
            by_age,
            long,
            trashed,
        ];
        // The pointer over a row, a tile, a status item, Cancel and Move.
        let mut hovered = Vec::new();
        for spot in 0..4 {
            let mut v = ready(policy(home, None), &cache);
            if spot == 3 {
                key(&mut v, KeyCode::Delete);
                age(&mut v);
            }
            text(&mut v, 120, 40);
            let (x, y) = match spot {
                0 => (v.hits.rows.x + 2, v.hits.rows.y + 1),
                1 => (v.hits.tiles[0].0.x + 1, v.hits.tiles[0].0.y),
                2 => (v.hits.items[0].0.x, v.hits.items[0].0.y),
                _ => (v.hits.buttons.unwrap().1.x + 1, v.hits.buttons.unwrap().1.y),
            };
            hover(&mut v, x, y);
            assert_ne!(v.hover, Hover::Nothing, "spot {spot}");
            hovered.push(v);
        }
        views.extend(hovered);
        views
    }

    fn hover(v: &mut View, x: u16, y: u16) -> Outcome {
        v.handle_mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        })
    }

    #[test]
    fn every_icon_set_and_colour_depth_draws_at_every_size() {
        use crate::style::{Colors, Depth};
        let (_t, home) = home("view-sets");
        let looks = [
            (IconSet::Nerd, Depth::True),
            (IconSet::Unicode, Depth::C256),
            (IconSet::Ascii, Depth::C16),
        ];
        for (set, depth) in looks {
            for v in &mut every_screen(&home) {
                v.set_icons(set);
                v.set_colors(Colors::default().fitted(depth));
                for w in 0..=60 {
                    for h in 0..=12 {
                        let area = Rect::new(0, 0, w, h);
                        v.draw(area, &mut Buffer::empty(area));
                    }
                }
                // Inside a host's larger buffer: nothing outside its area changes.
                for area in [
                    Rect::new(10, 5, 30, 8),
                    Rect::new(0, 0, 7, 3),
                    Rect::new(70, 25, 20, 10),
                    Rect::new(3, 2, 76, 26),
                ] {
                    let all = Rect::new(0, 0, 80, 30);
                    let mut buf = Buffer::empty(all);
                    for c in buf.content.iter_mut() {
                        c.set_symbol("X");
                    }
                    v.draw(area, &mut buf);
                    let mine = area.intersection(all);
                    for (i, c) in buf.content.iter().enumerate() {
                        let (x, y) = ((i % 80) as u16, (i / 80) as u16);
                        let inside =
                            x >= mine.x && x < mine.right() && y >= mine.y && y < mine.bottom();
                        assert!(inside || c.symbol() == "X", "({x}, {y}) for {area:?}");
                    }
                }
                let lines = text(v, 120, 40);
                if set == IconSet::Ascii {
                    let odd: Vec<&String> = lines.iter().filter(|l| !l.is_ascii()).collect();
                    assert!(odd.is_empty(), "ASCII only: {odd:#?}");
                }
            }
        }
    }

    #[test]
    fn hover_redraws_only_when_the_thing_under_the_pointer_changes() {
        let (_t, home) = home("view-hover");
        let mut v = ready(policy(&home, None), &home.join(".cache"));
        let buf = draw_buf(&mut v, 120, 40);
        let rows = v.hits.rows;
        let (x, y) = (rows.x + 3, rows.y + 2); // a row that is not selected
        assert_eq!(buf[(x, y)].bg, v.colors.side_bg);
        assert_eq!(hover(&mut v, x, y), Outcome::Redraw, "onto a row");
        assert_eq!(hover(&mut v, x + 5, y), Outcome::None, "along the same row");
        let buf = draw_buf(&mut v, 120, 40);
        assert_eq!(buf[(x, y)].bg, v.colors.hover_bg);
        assert_eq!(buf[(x, y + 1)].bg, v.colors.side_bg, "only that row");
        assert_eq!(
            hover(&mut v, x, y + 1),
            Outcome::Redraw,
            "onto the next row"
        );
        let tile = v.hits.tiles[0].0;
        assert_eq!(
            hover(&mut v, tile.x, tile.y),
            Outcome::Redraw,
            "onto a tile"
        );
        assert_eq!(hover(&mut v, tile.right() - 1, tile.y), Outcome::None);
        let item = v.hits.items[1].0;
        assert_eq!(hover(&mut v, item.x, item.y), Outcome::Redraw);
        let buf = draw_buf(&mut v, 120, 40);
        assert_eq!(buf[(item.x, item.y)].bg, v.colors.status_hover);
        // A drawing after a key keeps what is under the pointer, so a still pointer costs
        // nothing more.
        key(&mut v, KeyCode::Down);
        draw_buf(&mut v, 120, 40);
        assert_eq!(hover(&mut v, item.x, item.y), Outcome::None);
        // With the dialog open, the rows under it show no hover.
        key(&mut v, KeyCode::Delete);
        assert_eq!(hover(&mut v, x, y), Outcome::Redraw, "off the status item");
        let buf = draw_buf(&mut v, 120, 40);
        assert_eq!(v.hover, Hover::Nothing);
        assert_ne!(buf[(x, y)].bg, v.colors.hover_bg);
        // A click on a status item does what its key does.
        key(&mut v, KeyCode::Esc);
        draw_buf(&mut v, 120, 40);
        let by_type = v.hits.items[1];
        assert_eq!(by_type.1, 't');
        click(&mut v, by_type.0.x + 1, by_type.0.y);
        assert!(v.by_age);
        assert!(has(&text(&mut v, 120, 40), "t: by age"));
    }

    #[test]
    fn the_scrollbar_and_the_tile_outline_take_no_clicks_from_their_neighbours() {
        let (_t, home) = home("view-bars");
        let cache = home.join(".cache");
        let mut v = ready(policy(&home, None), &cache);
        let f0 = browse(&mut v)
            .tree
            .find(&cache.join("big/inner/f0"))
            .unwrap();
        browse(&mut v).select(f0); // big and inner open: more rows than the pane shows
        let buf = draw_buf(&mut v, 120, 12);
        let (rows, bar) = (v.hits.rows, v.hits.scrollbar);
        assert_eq!(bar.x, rows.right(), "the scrollbar is outside the rows");
        assert!(browse(&mut v).rows.len() > rows.height as usize);
        let slider = (bar.y..bar.bottom()).filter(|&y| buf[(bar.x, y)].symbol() == "┃");
        assert!(slider.count() >= 1, "the slider shows");
        // A click anywhere on the scrollbar column selects nothing and opens nothing.
        let (cursor, open) = (browse(&mut v).cursor, browse(&mut v).open.clone());
        for y in bar.y..bar.bottom() {
            v.last_click = None;
            assert_eq!(click(&mut v, bar.x, y), Outcome::None);
            let b = browse(&mut v);
            assert_eq!((b.cursor, &b.open), (cursor, &open), "row {y}");
        }
        // The wheel scrolls there too.
        v.handle_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: bar.x,
            row: bar.y,
            modifiers: KeyModifiers::NONE,
        });
        assert!(browse(&mut v).cursor < cursor);
        // Every cell of the selected tile's outline selects that tile, in every class mode.
        for t in [false, true] {
            let mut v = ready(policy(&home, None), &cache);
            v.by_age = t;
            select(&mut v, "big");
            let buf = draw_buf(&mut v, 120, 40);
            let b = browse(&mut v);
            let big = b.rows[b.cursor].0;
            let tile = v.hits.tiles.iter().find(|t| t.1 == Some(big)).unwrap().0;
            assert_eq!(buf[(tile.x, tile.y)].symbol(), "╭", "the outline");
            for (x, y) in (tile.x..tile.right())
                .flat_map(|x| [(x, tile.y), (x, tile.bottom() - 1)])
                .chain((tile.y..tile.bottom()).flat_map(|y| [(tile.x, y), (tile.right() - 1, y)]))
            {
                select(&mut v, "small");
                v.last_click = None;
                click(&mut v, x, y);
                assert_eq!(selected_name(&mut v), "big", "({x}, {y})");
            }
        }
    }

    #[test]
    fn the_map_shows_its_classes_by_letter_without_colour() {
        use crate::style::{Colors, Depth};
        let (_t, home) = home("view-classes");
        let cache = home.join(".cache");
        fs::write(cache.join("clip.MP4"), vec![0u8; 50_000]).unwrap();
        for (by_age, legend) in [
            (false, ["/ folder", "V video", "O other"]),
            (true, ["W this week"; 3]),
        ] {
            let mut v = ready(policy(&home, None), &cache);
            v.set_icons(IconSet::Ascii);
            v.set_colors(Colors::default().fitted(Depth::C16));
            v.by_age = by_age;
            let lines = text(&mut v, 120, 40);
            let map: Vec<String> = lines.iter().map(|l| l.chars().skip(61).collect()).collect();
            for l in legend {
                assert!(map[1].contains(l), "legend {l:?}: {:?}", map[1]);
            }
            for (r, id) in v.hits.tiles.clone() {
                let Some(id) = id else { continue };
                let name = browse(&mut v).tree.name(id).to_string_lossy().into_owned();
                let want = match (by_age, name.as_str()) {
                    (true, _) => "W",
                    (false, "big") => "/",
                    (false, "clip.MP4") => "V",
                    _ => "O",
                };
                let label: String = (r.x + 1..r.x + 3)
                    .map(|x| text_at(&lines, x, r.y))
                    .collect();
                assert_eq!(label, format!("{want} "), "{name}");
            }
        }
    }

    fn text_at(lines: &[String], x: u16, y: u16) -> String {
        lines[y as usize]
            .chars()
            .nth(x as usize)
            .unwrap()
            .to_string()
    }

    #[test]
    fn the_scan_card_turns_its_spinner_once_per_progress_redraw() {
        let (_t, home) = home("view-spin");
        let mut v = View::new(policy(&home, None), Some(&home));
        assert_eq!(v.next_wake(), Some(SCAN_CHECK));
        v.scan_started = Instant::now(); // the spinner's first frame, whatever the machine
        let first = text(&mut v, 120, 40);
        let line = |l: &[String]| l.iter().find(|l| l.contains(" files ")).unwrap().clone();
        assert!(
            line(&first).contains("⠋ ") && has(&first, "items/s"),
            "{first:#?}"
        );
        v.scan_started -= PROGRESS; // the next redraw, 100 ms later
        assert!(line(&text(&mut v, 120, 40)).contains("⠙ "));
        // The tree replaces it: no spinner, nothing to wake for.
        while v.next_wake().is_some() {
            v.tick();
            sleep(Duration::from_millis(1));
        }
        assert!(!has(&text(&mut v, 120, 40), "items/s"));
    }

    #[test]
    fn the_message_after_a_trash_stays_until_a_key_or_a_click() {
        let (_t, home) = home("view-done");
        let mut v = ready(policy(&home, None), &home.join(".cache"));
        select(&mut v, "small");
        key(&mut v, KeyCode::Delete);
        age(&mut v);
        key(&mut v, KeyCode::Tab);
        browse(&mut v).confirm.as_mut().unwrap().focused -= REFOCUS;
        key(&mut v, KeyCode::Enter);
        let msg = v.message.clone().unwrap();
        assert_eq!(
            msg,
            "Moved to the trash: small, 4.1 kB - freed when the trash is emptied"
        );
        assert_eq!(v.next_wake(), None, "no timer");
        let lines = text(&mut v, 120, 40);
        assert!(
            lines[39].starts_with(" ✓ Moved to the trash"),
            "in the status bar, with the set's check mark"
        );
        v.set_icons(IconSet::Ascii);
        assert!(text(&mut v, 120, 40)[39].starts_with(" + Moved"));
        hover(&mut v, 5, 5);
        assert!(v.message.is_some(), "the pointer does not dismiss it");
        let before = browse(&mut v).cursor;
        key(&mut v, KeyCode::Up);
        assert!(v.message.is_none() && !v.done, "a key does");
        assert_ne!(browse(&mut v).cursor, before, "and does its own work too");
    }

    #[test]
    fn a_refusal_after_a_trash_shows_without_the_check_mark() {
        let (_t, home) = home("view-done-refused");
        let mut v = ready(policy(&home, None), &home);
        select(&mut v, "Downloads");
        key(&mut v, KeyCode::Right);
        select(&mut v, "linked");
        key(&mut v, KeyCode::Delete);
        age(&mut v);
        key(&mut v, KeyCode::Tab);
        browse(&mut v).confirm.as_mut().unwrap().focused -= REFOCUS;
        key(&mut v, KeyCode::Enter);
        assert!(v.done);
        select(&mut v, ".config");
        key(&mut v, KeyCode::Delete);
        assert!(!v.done && v.message.as_ref().unwrap().contains("protected"));
        let line = &text(&mut v, 120, 40)[39];
        assert!(line.starts_with(" Cannot move to the trash"), "{line}");
    }

    fn draw_buf(v: &mut View, w: u16, h: u16) -> Buffer {
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        v.draw(area, &mut buf);
        buf
    }

    /// The text attributes of the cell in the middle of `r`.
    fn mods(buf: &Buffer, r: Rect) -> ratatui::style::Modifier {
        buf[(r.x + r.width / 2, r.y)].modifier
    }

    #[test]
    fn the_selection_the_lock_and_the_focus_show_without_colour() {
        use crate::style::{Colors, Depth};
        use ratatui::style::Modifier;
        let (_t, home) = home("view-cues");
        let c16 = Colors::default().fitted(Depth::C16);
        for (set, mark, lock) in [(IconSet::Unicode, "▌", "⊘"), (IconSet::Ascii, ">", "!")] {
            let mut v = ready(policy(&home, None), &home);
            v.set_icons(set);
            v.set_colors(c16);
            select(&mut v, ".config");
            let buf = draw_buf(&mut v, 120, 40);
            let rows = v.hits.rows;
            let b = browse(&mut v);
            let y = rows.y + (b.cursor - b.top) as u16;
            assert_eq!(
                buf[(rows.x, y)].symbol(),
                mark,
                "{set:?}: the selection's mark"
            );
            assert!(buf[(rows.x + 4, y)].modifier.contains(Modifier::BOLD));
            let row: String = (0..60).map(|x| buf[(x, y)].symbol()).collect();
            assert!(row.contains(&format!(".config/ {lock}")), "{set:?}: {row}");
            // The dialog: focus is bold, a Move to Trash that cannot be used is crossed out.
            let mut v = ready(policy(&home, None), &home.join(".cache"));
            v.set_icons(set);
            v.set_colors(c16);
            key(&mut v, KeyCode::Delete);
            let buf = draw_buf(&mut v, 120, 40);
            let (cancel, moving) = v.hits.buttons.unwrap();
            assert!(mods(&buf, cancel).contains(Modifier::BOLD));
            assert!(
                mods(&buf, moving).contains(Modifier::CROSSED_OUT),
                "not armed yet"
            );
            age(&mut v);
            let buf = draw_buf(&mut v, 120, 40);
            assert!(!mods(&buf, moving).intersects(Modifier::CROSSED_OUT | Modifier::BOLD));
            v.tick(); // draws the armed look, as the loop does
            key(&mut v, KeyCode::Tab);
            assert_eq!(v.next_wake(), None, "the keyboard's wait is not shown");
            let buf = draw_buf(&mut v, 120, 40);
            assert!(!mods(&buf, cancel).contains(Modifier::BOLD));
            let m = mods(&buf, moving);
            assert!(
                m.contains(Modifier::BOLD) && !m.contains(Modifier::CROSSED_OUT),
                "focused, and a click would move it: not drawn disabled"
            );
        }
    }

    /// The Move to Trash button in each state of the dialog: drawn disabled exactly when a
    /// click on it moves nothing, with the pointer resting on either button or on neither.
    #[test]
    fn rule7_drawn_disabled_exactly_when_a_click_moves_nothing() {
        use ratatui::style::Modifier;
        #[derive(Debug, Clone, Copy)]
        enum State {
            JustOpened,
            ArmedOnCancel,
            JustAfterTab,
            AfterRefocus,
            PathDoesNotFit,
        }
        for state in [
            State::JustOpened,
            State::ArmedOnCancel,
            State::JustAfterTab,
            State::AfterRefocus,
            State::PathDoesNotFit,
        ] {
            for hover in [None, Some(0), Some(1)] {
                let (_t, home) = home("view-states");
                let f = long_item(&home);
                let mut v = ready(policy(&home, None), &home.join(".cache"));
                ask_for(&mut v, &f);
                let c = browse(&mut v).confirm.as_mut().unwrap();
                match state {
                    State::JustOpened => {}
                    State::ArmedOnCancel | State::PathDoesNotFit => c.opened -= ARM,
                    State::JustAfterTab | State::AfterRefocus => {
                        c.opened -= ARM;
                        key(&mut v, KeyCode::Tab);
                    }
                }
                if let State::AfterRefocus = state {
                    browse(&mut v).confirm.as_mut().unwrap().focused -= REFOCUS;
                }
                // 60x8: room for the buttons, not for this path.
                let (w, h) = match state {
                    State::PathDoesNotFit => (60, 8),
                    _ => (120, 40),
                };
                draw_buf(&mut v, w, h);
                let (cancel, moving) = v.hits.buttons.unwrap();
                if let Some(i) = hover {
                    let r = [cancel, moving][i];
                    v.handle_mouse(MouseEvent {
                        kind: MouseEventKind::Moved,
                        column: r.x + 2,
                        row: r.y,
                        modifiers: KeyModifiers::NONE,
                    });
                }
                let buf = draw_buf(&mut v, w, h);
                let under = [Hover::Cancel, Hover::Move];
                assert_eq!(v.hover, hover.map_or(Hover::Nothing, |i| under[i]));
                let c = browse(&mut v).confirm.as_ref().unwrap();
                assert_eq!(c.fits, !matches!(state, State::PathDoesNotFit));
                let disabled = mods(&buf, moving).contains(Modifier::CROSSED_OUT);
                // A disabled button never takes the hover colour.
                let lit = buf[(moving.x + 2, moving.y)].bg;
                let k = v.colors;
                assert!(!(disabled && (lit == k.button_hover || lit == k.button2_hover)));
                v.last_click = None;
                click(&mut v, moving.x + 2, moving.y);
                let moved = !f.exists();
                assert_eq!(disabled, !moved, "{state:?}, pointer on {hover:?}");
                let expect = matches!(state, State::JustOpened | State::PathDoesNotFit);
                assert_eq!(disabled, expect, "{state:?}");
            }
        }
    }

    #[test]
    fn rule7_a_click_moves_it_only_once_armed() {
        let (_t, home) = home("view-click");
        let mut v = ready(policy(&home, None), &home.join(".cache"));
        key(&mut v, KeyCode::Delete);
        text(&mut v, 120, 40);
        let (_, moving) = v.hits.buttons.unwrap();
        click(&mut v, moving.x + 2, moving.y);
        assert!(browse(&mut v).confirm.is_some(), "too soon");
        browse(&mut v).confirm.as_mut().unwrap().opened -= Duration::from_secs(1);
        v.last_click = None;
        click(&mut v, moving.x + 2, moving.y);
        assert!(!home.join(".cache/big").exists());
    }

    #[test]
    fn rule8_read_only_has_no_delete_at_all() {
        let (_t, home) = home("view-ro");
        let p = policy(&home, Some("read-only (--read-only)".into()));
        let mut v = ready(p, &home.join(".cache"));
        key(&mut v, KeyCode::Delete);
        assert!(browse(&mut v).confirm.is_none() && v.message.is_none());
        let lines = text(&mut v, 120, 40);
        assert!(has(&lines, "read-only (--read-only)"));
        assert!(!has(&lines, "Del"), "no hint either");
    }

    #[test]
    fn rule9_no_delete_while_scanning() {
        let (_t, home) = home("view-scanning");
        let mut v = View::new(policy(&home, None), Some(&home.join(".cache")));
        assert_eq!(key(&mut v, KeyCode::Delete), Outcome::None);
        assert!(matches!(v.screen, Screen::Scanning(_)));
    }

    #[test]
    fn a_refused_item_shows_the_reason_and_a_lock() {
        let (_t, home) = home("view-refused");
        let mut v = ready(policy(&home, None), &home);
        select(&mut v, ".config");
        let lines = text(&mut v, 120, 40);
        assert!(
            has(&lines, ".config/ ⊘"),
            "the one-cell lock of the Unicode set"
        );
        key(&mut v, KeyCode::Delete);
        assert!(browse(&mut v).confirm.is_none());
        let msg = v.message.clone().unwrap();
        assert!(msg.contains("protected"), "{msg}");
        assert!(has(&text(&mut v, 120, 40), "protected"));
    }

    #[test]
    fn a_hard_link_says_moving_it_may_free_nothing() {
        let (_t, home) = home("view-link");
        let mut v = ready(policy(&home, None), &home.join(".cache"));
        select(&mut v, "linked");
        // 130 wide: at 120 the name column (24 cells beside Usage, Size and Modified) cuts it.
        assert!(has(&text(&mut v, 130, 40), "linked (hard link)"));
        key(&mut v, KeyCode::Delete);
        assert!(has(&text(&mut v, 120, 40), "may free nothing"));
    }

    /// The cell of the tree row named `name`, as drawn at 120x40.
    fn cell_of(v: &mut View, name: &str) -> (u16, u16) {
        text(v, 120, 40);
        let rows = v.hits.rows;
        let b = browse(v);
        let i = b
            .rows
            .iter()
            .position(|r| b.tree.name(r.0) == name)
            .unwrap();
        (rows.x + 3, rows.y + (i - b.top) as u16)
    }

    /// Whether a row named `name` is in the tree pane.
    fn row_shown(v: &mut View, name: &str) -> bool {
        let b = browse(v);
        b.rows.iter().any(|r| b.tree.name(r.0) == name)
    }

    #[test]
    fn folders_start_closed() {
        let (_t, home) = home("view-closed");
        let mut v = ready(policy(&home, None), &home);
        let b = browse(&mut v);
        assert_eq!(b.open, HashSet::from([0]), "only the shown folder is open");
        assert!(b
            .rows
            .iter()
            .all(|r| (r.1 == 0) == (r.0 == b.zoom) && r.1 <= 1));
        assert!(row_shown(&mut v, ".cache") && !row_shown(&mut v, "big"));
        let lines = text(&mut v, 120, 40);
        assert!(has(&lines, "▸ .cache/") && !has(&lines, "big/"));
    }

    #[test]
    fn tree_and_map_select_each_other_and_a_double_click_opens_and_closes() {
        let (_t, home) = home("view-mouse");
        let mut v = ready(policy(&home, None), &home.join(".cache"));
        text(&mut v, 120, 40);
        let small = browse(&mut v)
            .tree
            .find(&home.join(".cache/small"))
            .unwrap();
        let tile = v.hits.tiles.iter().find(|t| t.1 == Some(small)).unwrap().0;
        click(&mut v, tile.x, tile.y);
        assert_eq!(selected_name(&mut v), "small", "a tile selects its row");
        select(&mut v, "big");
        let b = browse(&mut v);
        let big = b.rows[b.cursor].0;
        assert_eq!(b.map_of(), (0, Some(big)), "the row selects its tile");
        // A double click on the "big" row opens it where it is; another closes it.
        let (x, y) = cell_of(&mut v, "big");
        click(&mut v, x, y);
        click(&mut v, x, y);
        assert_eq!(browse(&mut v).zoom, 0, "the shown folder stays");
        assert!(browse(&mut v).open.contains(&big) && row_shown(&mut v, "inner"));
        assert!(has(&text(&mut v, 120, 40), "▾ big/"));
        assert_eq!(selected_name(&mut v), "big");
        click(&mut v, x, y);
        click(&mut v, x, y);
        assert!(!browse(&mut v).open.contains(&big) && !row_shown(&mut v, "inner"));
        assert_eq!(selected_name(&mut v), "big");
        // Enter still shows that folder, Backspace the one above.
        key(&mut v, KeyCode::Enter);
        assert_eq!(browse(&mut v).zoom, big);
        assert_eq!(selected_name(&mut v), "inner");
        key(&mut v, KeyCode::Backspace);
        assert_eq!(browse(&mut v).zoom, 0);
        assert_eq!(selected_name(&mut v), "big");
        // A double click on a tile still shows that folder.
        text(&mut v, 120, 40);
        let tile = v.hits.tiles.iter().find(|t| t.1 == Some(big)).unwrap().0;
        click(&mut v, tile.x, tile.y);
        click(&mut v, tile.x, tile.y);
        assert_eq!(browse(&mut v).zoom, big);
    }

    #[test]
    fn a_double_click_on_a_file_or_on_the_shown_folder_changes_nothing() {
        let (_t, home) = home("view-dbl-file");
        let mut v = ready(policy(&home, None), &home.join(".cache"));
        let rows = browse(&mut v).rows.clone();
        for name in ["small", ".cache"] {
            let name = if name == ".cache" {
                home.join(".cache").display().to_string() // the shown folder's row: its path
            } else {
                name.to_string()
            };
            let (x, y) = cell_of(&mut v, &name);
            click(&mut v, x, y);
            click(&mut v, x, y);
            let b = browse(&mut v);
            assert_eq!((b.rows.clone(), b.zoom), (rows.clone(), 0), "{name}");
            assert_eq!(b.open, HashSet::from([0]));
        }
    }

    #[test]
    fn a_triple_click_opens_once_on_the_row_clicked() {
        let (_t, home) = home("view-triple");
        let mut v = ready(policy(&home, None), &home.join(".cache"));
        let (x, y) = cell_of(&mut v, "big");
        for _ in 0..3 {
            click(&mut v, x, y);
        }
        assert!(
            row_shown(&mut v, "inner"),
            "opened by the double click, not closed again"
        );
        assert_eq!(
            selected_name(&mut v),
            "big",
            "the third click selects the same row"
        );
    }

    #[test]
    fn closing_a_folder_brings_the_selection_up_to_it() {
        let (_t, home) = home("view-close-sel");
        let mut v = ready(policy(&home, None), &home.join(".cache"));
        let b = browse(&mut v);
        let (big, f0) = (
            b.tree.find(&home.join(".cache/big")).unwrap(),
            b.tree.find(&home.join(".cache/big/inner/f0")).unwrap(),
        );
        b.select(f0); // opens big and inner
        assert_eq!(selected_name(&mut v), "f0");
        let b = browse(&mut v);
        b.open.remove(&big);
        b.rebuild(Some(f0));
        assert_eq!(selected_name(&mut v), "big", "not a row that is gone");
        assert_eq!(browse(&mut v).map_of(), (0, Some(big)), "the map follows");
        // The same through the mouse: the click lands on big first, the double click closes it.
        browse(&mut v).select(f0);
        let (x, y) = cell_of(&mut v, "big");
        click(&mut v, x, y);
        click(&mut v, x, y);
        assert_eq!(selected_name(&mut v), "big");
        assert!(!row_shown(&mut v, "f0"));
    }

    /// The names on the map's tiles, and those drawn in the selected tile's colour.
    fn tiles(v: &mut View, w: u16, h: u16) -> (Vec<String>, Vec<String>) {
        let buf = draw_buf(v, w, h);
        let blue = v.colors.sel_tile;
        let hits = v.hits.tiles.clone();
        let b = browse(v);
        let name = |id: Option<NodeId>| match id {
            Some(id) => b.tree.name(id).to_string_lossy().into_owned(),
            None => "+".to_string(),
        };
        let all = hits.iter().map(|t| name(t.1)).collect();
        let lit = hits
            .iter()
            .filter(|t| buf[(t.0.x, t.0.y)].bg == blue)
            .map(|t| name(t.1))
            .collect();
        (all, lit)
    }

    #[test]
    fn the_map_shows_the_folder_of_the_selected_row_and_only_its_tile_in_blue() {
        let (_t, home) = home("view-map-follows");
        let mut v = ready(policy(&home, None), &home.join(".cache"));
        assert_eq!(selected_name(&mut v), "big");
        let (all, lit) = tiles(&mut v, 120, 40);
        assert_eq!(all, ["big", "linked", "small"]);
        assert_eq!(lit, ["big"]);
        assert!(has(&text(&mut v, 120, 40), "100% ━"), "root row");
        // Into big: the map shows big's items, the selected one in blue.
        key(&mut v, KeyCode::Right);
        key(&mut v, KeyCode::Down);
        assert_eq!(selected_name(&mut v), "inner");
        let (all, lit) = tiles(&mut v, 120, 40);
        assert!(all == ["inner"] && lit == ["inner"], "{all:?} {lit:?}");
        let map: Vec<String> = text(&mut v, 120, 40)
            .iter()
            .map(|l| l.chars().skip(61).collect())
            .collect();
        let legend = &map[1];
        assert!(legend.starts_with(" big/ "), "{legend:?}");
        // Into inner: its files, each tile in a colour of its own but for the selected one.
        key(&mut v, KeyCode::Right);
        key(&mut v, KeyCode::Down);
        let first = selected_name(&mut v);
        let (all, lit) = tiles(&mut v, 120, 40);
        assert_eq!((all.len(), lit), (20, vec![first.clone()]));
        let buf = draw_buf(&mut v, 120, 40);
        let hits = v.hits.tiles.clone();
        let bgs: Vec<_> = hits.iter().map(|t| buf[(t.0.x, t.0.y)].bg).collect();
        assert!(bgs.windows(2).all(|w| w[0] != w[1]), "{bgs:?}");
        // A click on another of its tiles selects that row; the map stays on inner.
        let (r, id) = hits[19];
        click(&mut v, r.x + 1, r.y);
        let b = browse(&mut v);
        assert_eq!(b.rows[b.cursor].0, id.unwrap(), "the row of that tile");
        let name = selected_name(&mut v);
        assert_ne!(name, first);
        assert_eq!(tiles(&mut v, 120, 40).1, [name]);
        // Too small for tiles of their own: the "+20 items" tile holds the selected one.
        v.panes = Panes::Map;
        let (all, lit) = tiles(&mut v, 24, 6);
        assert!(all == ["+"] && lit == ["+"], "{all:?} {lit:?}");
        v.panes = Panes::Both;
        // On the shown folder's row: its items, none selected.
        key(&mut v, KeyCode::Home);
        let (all, lit) = tiles(&mut v, 120, 40);
        assert_eq!((all.len(), lit.len()), (3, 0));
    }

    #[test]
    fn with_tiles_and_small_items_together_only_the_selected_one_is_blue() {
        let (_t, home) = home("view-mixed");
        let mixed = home.join(".cache/mixed");
        fs::create_dir(&mixed).unwrap();
        fs::write(mixed.join("a"), vec![0u8; 120_000]).unwrap();
        fs::write(mixed.join("b"), vec![0u8; 100_000]).unwrap();
        for i in 0..20 {
            fs::write(mixed.join(format!("s{i}")), vec![0u8; 3000]).unwrap();
        }
        let mut v = ready(policy(&home, None), &mixed);
        key(&mut v, KeyCode::Char('a')); // apparent sizes: the lengths, whatever the disk does
        v.panes = Panes::Map;
        // 40x12: a and b get tiles, the twenty small files one "+20 items" tile.
        for (name, blue) in [("b", "b"), ("s7", "+"), ("a", "a"), ("s0", "+")] {
            select(&mut v, name);
            let (all, lit) = tiles(&mut v, 40, 12);
            assert_eq!(all, ["a", "b", "+"]);
            assert_eq!(lit, [blue], "{name}");
        }
    }

    /// The names of the rows under the shown folder, in the tree's order.
    fn order(v: &mut View) -> Vec<String> {
        let b = browse(v);
        let names = b.rows[1..].iter().map(|r| b.tree.name(r.0));
        names.map(|n| n.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn apparent_sizes_reorder_a_tree_sorted_by_size() {
        let (_t, home) = home("view-apparent-order");
        let dir = home.join(".cache/sizes");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("dense"), vec![1u8; 50_000]).unwrap();
        // A hole: a million bytes long, no blocks on the disk.
        let sparse = fs::File::create(dir.join("sparse")).unwrap();
        sparse.set_len(1_000_000).unwrap();
        let mut v = ready(policy(&home, None), &dir);
        assert_eq!(order(&mut v), ["dense", "sparse"], "by disk usage");
        key(&mut v, KeyCode::Char('a'));
        assert_eq!(browse(&mut v).sort, Sort::default());
        assert_eq!(order(&mut v), ["sparse", "dense"], "by apparent size");
        key(&mut v, KeyCode::Char('a'));
        assert_eq!(order(&mut v), ["dense", "sparse"]);
    }

    #[test]
    fn a_narrow_tree_keeps_the_column_it_is_sorted_by() {
        let (_t, home) = home("view-sorted-column");
        let mut v = ready(policy(&home, None), &home.join(".cache"));
        key(&mut v, KeyCode::Char('s'));
        assert!(has(&text(&mut v, 120, 40), "Items▾"));
        let heads: Vec<Field> = v.hits.heads.iter().map(|h| h.1).collect();
        assert_eq!(heads, [Field::Name, Field::Size, Field::Size, Field::Items]);
        // Every sort at every width: the headings stay in the tree pane, the names keep 24 cells
        // (the Name heading starts a cell in) beside any other column, and the sorted column is
        // there wherever it fits beside them.
        for _ in 0..4 {
            key(&mut v, KeyCode::Char('s'));
            let by = browse(&mut v).sort.by;
            for w in 24..=140 {
                text(&mut v, w, 12);
                let (heads, pane) = (v.hits.heads.clone(), v.hits.rows);
                let at = format!("{by:?} at {w}: {heads:?}");
                assert!(heads.iter().all(|h| h.0.right() <= pane.right()), "{at}");
                assert!(heads.len() == 1 || heads[0].0.width >= 23, "{at}");
                assert!(w < 40 || heads.iter().any(|h| h.1 == by), "{at}");
            }
        }
    }

    #[test]
    fn the_tree_sorts_by_each_column_both_ways_and_keeps_the_selection() {
        let (_t, home) = home("view-sort");
        let cache = home.join(".cache");
        for (name, secs) in [("linked", 1_000), ("big", 2_000), ("small", 3_000)] {
            let t = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
            fs::File::open(cache.join(name))
                .unwrap()
                .set_modified(t)
                .unwrap();
        }
        let mut v = ready(policy(&home, None), &cache);
        assert_eq!(browse(&mut v).sort, Sort::default());
        assert_eq!(order(&mut v), ["big", "linked", "small"], "largest first");
        select(&mut v, "linked");
        // big, the one folder, comes first either way; the files hold no items, so they stay
        // largest first by items.
        let steps = [
            ('s', Field::Items, true, ["big", "linked", "small"]),
            ('S', Field::Items, false, ["big", "linked", "small"]),
            ('s', Field::Modified, true, ["big", "small", "linked"]),
            ('S', Field::Modified, false, ["big", "linked", "small"]),
            ('s', Field::Name, false, ["big", "linked", "small"]),
            ('S', Field::Name, true, ["big", "small", "linked"]),
            ('s', Field::Size, true, ["big", "linked", "small"]),
            ('S', Field::Size, false, ["big", "small", "linked"]),
        ];
        for (k, by, descending, rows) in steps {
            key(&mut v, KeyCode::Char(k));
            assert_eq!(browse(&mut v).sort, Sort { by, descending });
            assert_eq!(order(&mut v), rows, "{by:?}, descending {descending}");
            assert_eq!(selected_name(&mut v), "linked", "selection kept");
        }
        // A click on a heading sorts by it, the next one turns it round; Usage is the size's.
        text(&mut v, 120, 40);
        let heads = v.hits.heads.clone();
        assert_eq!(
            heads.iter().map(|h| h.1).collect::<Vec<_>>(),
            [Field::Name, Field::Size, Field::Size, Field::Modified],
            "Name, Usage, Size, Modified (Items has no room)"
        );
        let (name, usage) = (heads[0].0, heads[1].0);
        assert_eq!(v.hover_at(name.x + 1, name.y), Hover::Head(0));
        click(&mut v, name.x + 1, name.y);
        assert_eq!(browse(&mut v).sort, Sort::default().by(Field::Name));
        assert!(has(&text(&mut v, 120, 40), "Name▴"));
        click(&mut v, name.x + 1, name.y);
        assert_eq!(order(&mut v), ["big", "small", "linked"]);
        assert!(has(&text(&mut v, 120, 40), "Name▾"));
        click(&mut v, usage.x + 1, usage.y);
        assert_eq!(browse(&mut v).sort, Sort::default());
        assert!(has(&text(&mut v, 120, 40), "Size▾"));
        // The sort stays when the sizes switch to apparent ones; the map keeps its own order,
        // largest first.
        key(&mut v, KeyCode::Char('s'));
        key(&mut v, KeyCode::Char('s'));
        key(&mut v, KeyCode::Char('s'));
        assert_eq!(order(&mut v), ["big", "linked", "small"], "by name");
        key(&mut v, KeyCode::Char('a'));
        assert_eq!(browse(&mut v).sort.by, Field::Name);
        key(&mut v, KeyCode::Char('S'));
        assert_eq!(order(&mut v), ["big", "small", "linked"]);
        assert_eq!(tiles(&mut v, 120, 40).0, ["big", "linked", "small"]);
    }

    #[test]
    fn folders_come_first_in_every_order_and_the_map_stays_by_size() {
        let (_t, home) = home("view-folders-first");
        let dir = home.join(".cache/mix");
        for d in ["d_big", "D_small"] {
            fs::create_dir_all(dir.join(d)).unwrap();
        }
        fs::write(dir.join("d_big/x"), vec![0u8; 50_000]).unwrap();
        fs::write(dir.join("D_small/x"), vec![0u8; 500]).unwrap();
        fs::write(dir.join("D_small/y"), vec![0u8; 500]).unwrap();
        fs::write(dir.join("f_big"), vec![0u8; 80_000]).unwrap();
        fs::write(dir.join("F_small"), vec![0u8; 2000]).unwrap();
        let times = [
            ("d_big", 1_000),
            ("D_small", 2_000),
            ("f_big", 3_000),
            ("F_small", 1_500),
        ];
        for (name, secs) in times {
            let t = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
            fs::File::open(dir.join(name))
                .unwrap()
                .set_modified(t)
                .unwrap();
        }
        let mut v = ready(policy(&home, None), &dir);
        let (d, dd, f, ff) = ("d_big", "D_small", "f_big", "F_small");
        assert_eq!(order(&mut v), [d, dd, f, ff], "size, largest first");
        select(&mut v, ff);
        // The folders hold 1 and 2 items, the files none (so they stay largest first).
        let steps = [
            ('s', Field::Items, true, [dd, d, f, ff]),
            ('S', Field::Items, false, [d, dd, f, ff]),
            ('s', Field::Modified, true, [dd, d, f, ff]),
            ('S', Field::Modified, false, [d, dd, ff, f]),
            ('s', Field::Name, false, [d, dd, f, ff]),
            ('S', Field::Name, true, [dd, d, ff, f]),
            ('s', Field::Size, true, [d, dd, f, ff]),
            ('S', Field::Size, false, [dd, d, ff, f]),
        ];
        for (k, by, descending, rows) in steps {
            key(&mut v, KeyCode::Char(k));
            assert_eq!(browse(&mut v).sort, Sort { by, descending });
            assert_eq!(order(&mut v), rows, "{by:?}, descending {descending}");
            assert_eq!(selected_name(&mut v), ff, "selection kept");
            assert_eq!(tiles(&mut v, 120, 40).0, [f, d, dd, ff], "the map: by size");
        }
    }

    /// The line of the tree row named `name` in the last drawing.
    fn row_y(v: &mut View, name: &str) -> u16 {
        let rows = v.hits.rows;
        let b = browse(v);
        let i = b.rows.iter().position(|r| b.tree.name(r.0) == name);
        rows.y + (i.unwrap() - b.top) as u16
    }

    #[test]
    fn the_tree_colours_each_level_and_the_selected_row_keeps_its_own() {
        let (_t, home) = home("view-levels");
        let deep = home.join(".cache/big/inner/deep");
        fs::create_dir(&deep).unwrap();
        fs::write(deep.join("leaf"), b"x").unwrap();
        for (set, guide, open) in [(IconSet::Unicode, "│", "▾"), (IconSet::Ascii, "|", "v")] {
            let mut v = ready(policy(&home, None), &home.join(".cache"));
            v.set_icons(set);
            let c = v.colors;
            let leaf = browse(&mut v).tree.find(&deep.join("leaf")).unwrap();
            browse(&mut v).select(leaf); // opens big, inner and deep
            select(&mut v, "linked"); // the selection away from the rows looked at
            let buf = draw_buf(&mut v, 120, 40);
            // Where names start.
            let x = v.hits.rows.x + 1;
            // The shown folder's row: no guide, its chevron in the first level's colour.
            let top = v.hits.rows.y;
            assert_eq!(buf[(x, top)].symbol(), open, "{set:?}");
            assert_eq!(buf[(x, top)].fg, c.depth[0], "{set:?}");
            for (name, depth) in [("big", 1), ("inner", 2), ("deep", 3), ("leaf", 4)] {
                let y = row_y(&mut v, name);
                for level in 0..depth {
                    let cell = &buf[(x + 2 * level, y)];
                    let at = format!("{set:?} {name}, level {level}");
                    assert_eq!(cell.symbol(), guide, "{at}");
                    assert_eq!(cell.fg, c.depth[usize::from(level) % 3], "{at}");
                    assert_eq!(buf[(x + 2 * level + 1, y)].symbol(), " ", "{at}");
                }
                // A folder's chevron in its own level's colour; a file's icon as before.
                let (at, fg) = match name {
                    "leaf" => (x + 2 * depth + 2, c.icon_fg),
                    _ => (x + 2 * depth, c.depth[usize::from(depth) % 3]),
                };
                if set == IconSet::Unicode || name != "leaf" {
                    assert_eq!(buf[(at, y)].fg, fg, "{set:?} {name}");
                }
            }
            // The selected row keeps its colours, guides and all.
            browse(&mut v).select(leaf);
            let buf = draw_buf(&mut v, 120, 40);
            let y = row_y(&mut v, "leaf");
            assert_eq!(buf[(x, y)].symbol(), guide);
            for cell in (x..x + 12).map(|x| &buf[(x, y)]) {
                assert_eq!((cell.fg, cell.bg), (c.sel_fg, c.sel_bg), "{set:?}");
            }
        }
    }

    #[test]
    fn colours_on_a_deep_row_stop_where_its_text_is_cut() {
        let (_t, home) = home("view-levels-cut");
        let mut dir = home.join(".cache");
        for i in 0..10 {
            dir = dir.join(format!("n{i}"));
        }
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("leafz"), b"x").unwrap();
        let mut v = ready(policy(&home, None), &home.join(".cache"));
        let leaf = browse(&mut v).tree.find(&dir.join("leafz")).unwrap();
        browse(&mut v).select(leaf); // 11 levels deep: 22 cells of guides
        key(&mut v, KeyCode::Up);
        v.panes = Panes::Tree;
        let buf = draw_buf(&mut v, 24, 30);
        let (x, y) = (v.hits.rows.x + 1, row_y(&mut v, "leafz"));
        // 21 cells for the text: 20 of it, then the cut mark, which no guide covers.
        assert_eq!(buf[(x + 20, y)].symbol(), "…");
        assert_eq!(buf[(x + 18, y)].symbol(), "│");
        assert_eq!(buf[(x + 18, y)].fg, v.colors.depth[9 % 3]);
    }

    #[test]
    fn an_open_folder_stays_open_after_a_trash_inside_it_and_closes_on_rescan() {
        let (_t, home) = home("view-open-trash");
        let mut v = ready(policy(&home, None), &home.join(".cache"));
        let inner = browse(&mut v)
            .tree
            .find(&home.join(".cache/big/inner"))
            .unwrap();
        browse(&mut v).select(inner);
        key(&mut v, KeyCode::Delete);
        age(&mut v);
        key(&mut v, KeyCode::Tab);
        browse(&mut v).confirm.as_mut().unwrap().focused -= REFOCUS;
        key(&mut v, KeyCode::Enter);
        assert!(!home.join(".cache/big/inner").exists());
        assert!(has(&text(&mut v, 120, 40), "▾ big/"), "still open");
        assert!(!row_shown(&mut v, "inner"));
        let b = browse(&mut v);
        assert!(b.tree.node(b.rows[b.cursor].0).flags & TRASHED == 0);
        // A rescan builds a new tree: folders start closed again.
        key(&mut v, KeyCode::Char('r'));
        for _ in 0..5000 {
            v.tick();
            if matches!(v.screen, Screen::Browse(_)) {
                break;
            }
            sleep(Duration::from_millis(1));
        }
        assert_eq!(browse(&mut v).open, HashSet::from([0]));
    }

    #[test]
    fn panes_and_size_kind_switch() {
        let (_t, home) = home("view-keys");
        let mut v = ready(policy(&home, None), &home.join(".cache"));
        key(&mut v, KeyCode::Char('v'));
        assert_eq!(v.panes, Panes::Tree);
        assert!(has(&text(&mut v, 120, 40), "Modified") && v.hits.tiles.is_empty());
        key(&mut v, KeyCode::Char('v'));
        assert!(!has(&text(&mut v, 120, 40), "Modified"), "map only");
        assert!(!v.hits.tiles.is_empty());
        key(&mut v, KeyCode::Char('a'));
        assert!(has(&text(&mut v, 120, 40), "apparent sizes"));
        assert_eq!(key(&mut v, KeyCode::Char('q')), Outcome::Quit);
        let copied = key(&mut v, KeyCode::Char('c'));
        assert_eq!(copied, Outcome::CopyPath(home.join(".cache/big")));
    }

    #[test]
    fn the_start_screen_offers_four_places() {
        let (_t, home) = home("view-start");
        let mut v = View::new(policy(&home, None), None);
        let lines = text(&mut v, 120, 40);
        for s in [
            "Home",
            "Cache",
            "Temporary files",
            "Trash",
            "Home filesystem",
        ] {
            assert!(has(&lines, s), "{s}: {lines:?}");
        }
        assert_eq!(v.next_wake(), None, "idle: nothing to wake for");
        key(&mut v, KeyCode::Down);
        key(&mut v, KeyCode::Enter);
        assert!(
            matches!(v.screen, Screen::Scanning(_)),
            "the cache is scanned"
        );
    }

    /// Prints the start screen, the tree and the dialog at 120x40, in Unicode and in ASCII,
    /// to look at: `cargo test -- --ignored --nocapture print_screens`.
    #[test]
    #[ignore = "prints screens to look at"]
    fn print_screens() {
        let (_t, home) = home("view-print");
        let day = Duration::from_secs(86_400);
        for (dir, size, days) in [
            ("Videos/trip.mp4", 2_400_000, 3),
            ("projects/app/target/debug.bin", 1_300_000, 20),
            (".local/share/app/data.db", 700_000, 200),
            ("Downloads/linux.iso", 1_800_000, 400),
            ("Downloads/clip.mkv", 900_000, 2),
            ("Downloads/photo.jpg", 600_000, 15),
            ("Downloads/song.mp3", 450_000, 100),
            ("Downloads/notes.pdf", 300_000, 800),
            ("Downloads/data.bin", 200_000, 50),
            ("Downloads/script.py", 120_000, 1),
        ] {
            let p = home.join(dir);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, vec![0u8; size]).unwrap();
            let f = fs::File::options().write(true).open(&p).unwrap();
            f.set_modified(std::time::SystemTime::now() - day * days)
                .unwrap();
        }
        let show = |title: &str, v: &mut View| {
            println!("--- {title}");
            for l in text(v, 120, 40) {
                println!("{}", l.trim_end());
            }
        };
        for set in [IconSet::Unicode, IconSet::Ascii] {
            let mut start = View::new(policy(&home, None), None);
            start.set_icons(set);
            show(&format!("start, {set:?}"), &mut start);
            let mut main = ready(policy(&home, None), &home);
            main.set_icons(set);
            select(&mut main, "Downloads");
            key(&mut main, KeyCode::Enter);
            show(&format!("tree, by type, {set:?}"), &mut main);
            key(&mut main, KeyCode::Char('t'));
            show(&format!("tree, by age, {set:?}"), &mut main);
            let mut scanning = View::new(policy(&home, None), Some(&home));
            scanning.set_icons(set);
            show(&format!("scanning, {set:?}"), &mut scanning);
            let mut d = ready(policy(&home, None), &home.join(".cache"));
            d.set_icons(set);
            key(&mut d, KeyCode::Delete);
            show(&format!("dialog just opened, {set:?}"), &mut d);
            age(&mut d);
            key(&mut d, KeyCode::Tab);
            browse(&mut d).confirm.as_mut().unwrap().focused -= REFOCUS;
            key(&mut d, KeyCode::Enter);
            show(&format!("after a trash, {set:?}"), &mut d);
        }
    }
}
