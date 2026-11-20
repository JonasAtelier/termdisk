// SPDX-License-Identifier: GPL-3.0-only

//! The embeddable view: what is on screen and what keys and clicks do to it. It draws into a
//! ratatui `Buffer` (draw.rs) and takes crossterm events, both only as data: the terminal itself
//! belongs to the host (the CLI in main.rs, or termcode).

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::mem;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;

use crate::draw::count;
use crate::guard::{self, Policy, Refusal};
use crate::style::{Colors, IconSet};
use crate::sys;
use crate::trash::{Emptied, Tally};
use crate::tree::{Node, NodeId, Tree, HARDLINK_DUP, HOLDS, LOCKED, TRASHED};
use crate::tree::{MOUNT, NOT_OWNED, PROTECTED, UNREADABLE};
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

/// How long a new confirmation dialog can do nothing but close (keys typed ahead, a key held
/// down): Tab moves the focus at once, but Enter on "Move to Trash" does nothing until then.
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
    /// The trash is being scanned from the start screen's Del: its tree opens with the dialog
    /// to empty it.
    empty_next: bool,
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
    /// The close button.
    Close,
    /// The lock of this item, on its row or its tile (`Hits::locks`): a box says why it is
    /// locked. A click there is a click on that row or tile.
    Lock(NodeId),
    /// The tick box of this item's row (`Hits::boxes`).
    Box(NodeId),
}

pub(crate) enum Screen {
    Start(Start),
    Scanning(Scan),
    Browse(Box<Browse>),
    /// The tree's folder scanned again (after a trash, or `r`). The last tree went when this
    /// scan started, so two trees never share the memory (that doubled the peak); what it
    /// showed waits by path, and `tick` shows the new tree that way (`Browse::restore`). Drawn
    /// as a scan; keys and clicks wait but Esc, which stops it (back to the places, as for a
    /// first scan: the last tree is gone), and `q`.
    Refreshing(Scan, Saved),
    /// Only while one screen is being swapped for another.
    Gone,
}

/// What a tree showed, by path below its root (`Browse::save`), to show the next scan of the
/// same folder the same way (`Browse::restore`).
pub(crate) struct Saved {
    /// The scanned folder, for the title meanwhile.
    pub root: PathBuf,
    zoom: PathBuf,
    selected: PathBuf,
    /// The selected row's line in the tree pane.
    line: usize,
    open: Vec<PathBuf>,
    ticked: Vec<PathBuf>,
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
    /// The ticked items (Space, a drag over the rows): Del moves these. Kept whatever is shown.
    /// Only the top ones: an item in a ticked folder shows ticked and goes with it (`tidy`).
    pub ticked: HashSet<NodeId>,
    /// A drag over the rows going on: where it started and what was ticked before it.
    drag: Option<Drag>,
    /// The trash being emptied: what has gone so far (the tree is the one from before).
    pub emptying: Option<Emptying>,
}

/// The trash being emptied, on a thread of its own. Dropping it (a host drops the view) stops
/// it at the next entry, as dropping a `Scan` cancels it.
pub(crate) struct Emptying {
    pub tally: Arc<Tally>,
    /// The items the trash held, as the scan saw it.
    pub of: usize,
    /// Taken once it has finished.
    job: Option<JoinHandle<Emptied>>,
}

impl Emptying {
    fn finished(&self) -> bool {
        self.job.as_ref().is_none_or(JoinHandle::is_finished)
    }
}

impl Drop for Emptying {
    fn drop(&mut self) {
        self.tally.stop.store(true, Relaxed);
    }
}

/// A drag with the left button over the tree's rows: it ticks the rows from the `anchor` item's
/// to the one under the pointer, on top of `before`, once it has left the row it started on;
/// started on a tick box, it gives them the state that box took (`on`). Any key and any new
/// layout of the rows end it (`before` and the rows would be stale).
struct Drag {
    anchor: NodeId,
    before: HashSet<NodeId>,
    moved: bool,
    on: bool,
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
    /// What goes, in this order, and the path shown for each. One item without refused ones
    /// is the dialog for one; more (the ticked items) list every path.
    pub items: Vec<(NodeId, PathBuf)>,
    /// Ticked items the guard refused when the dialog opened, and why: listed, not moved.
    pub refused: Vec<(PathBuf, String)>,
    /// The items are the ticked ones, not the selected row (the dialog for one says so).
    pub ticked: bool,
    /// The dialog empties the trash (its one item is the trash's folder) for good.
    pub empty: bool,
    pub opened: Instant,
    /// The focus is on "Move to Trash" (it starts on Cancel).
    pub on_move: bool,
    /// When the focus last moved.
    pub focused: Instant,
    /// The armed look has been drawn.
    pub shown_armed: bool,
    /// The last drawing showed the whole path, and for several items every path has been
    /// shown whole once; until then (and while it cannot) nothing can be moved.
    pub fits: bool,
    /// The list of several paths: its first line on screen; its lines on screen (PgUp); the
    /// first line of the next page, which never splits a path that the last page cut (PgDn,
    /// so reading page by page shows every path whole); whether each path has been shown
    /// whole.
    pub scroll: usize,
    pub page: usize,
    pub next_page: usize,
    pub seen: Vec<bool>,
}

impl Confirm {
    fn new(items: Vec<(NodeId, PathBuf)>, refused: Vec<(PathBuf, String)>) -> Self {
        let now = Instant::now();
        Confirm {
            seen: vec![false; items.len()],
            items,
            refused,
            ticked: false,
            empty: false,
            opened: now,
            on_move: false,
            focused: now,
            shown_armed: false,
            fits: false,
            scroll: 0,
            page: 1,
            next_page: 1,
        }
    }

    /// The dialog for one item, as without ticks.
    pub fn one(&self) -> bool {
        self.items.len() == 1 && self.refused.is_empty()
    }

    pub fn armed(&self) -> bool {
        self.opened.elapsed() >= ARM
    }

    /// Whether a click on "Move to Trash" may move the item: armed and the whole path shown.
    /// The button is drawn disabled exactly when this is false (the keyboard's `REFOCUS` wait
    /// is not shown); the focus may be on it meanwhile.
    pub fn clickable(&self) -> bool {
        self.armed() && self.fits
    }

    /// Whether Enter on "Move to Trash" may move the item now: armed, the whole path shown,
    /// and the focus there for `REFOCUS` (a Tab and an Enter in one burst, as pasted text
    /// brings them where bracketed paste is missing, never confirm). Until then Enter there
    /// does nothing and the dialog stays.
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
    /// The close button at the right of the title bar: a click on it presses `q`.
    pub close: Option<Rect>,
    /// Each lock drawn, with the cell before it, and its item: the pointer there shows why.
    pub locks: Vec<(Rect, NodeId)>,
    /// Each tick box drawn whole (not the shown folder's), and its item: a click ticks it.
    pub boxes: Vec<(Rect, NodeId)>,
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
            empty_next: false,
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

    /// Scans the tree's folder again (`Screen::Refreshing`): what the tree shows is kept by path
    /// and the tree itself goes as the scan starts. If it cannot be scanned, nothing has gone:
    /// the tree stays and the message says so (after what a trash just did, if it says that).
    // ponytail: the whole folder, not only the one a trash changed: 0.3 s for 1.2 million
    // entries warm. Rescan one folder and splice it in only if that ever shows (hard links
    // across it, every NodeId held by the view).
    fn refresh(&mut self) {
        let b = match mem::replace(&mut self.screen, Screen::Gone) {
            Screen::Browse(b) => b,
            other => {
                self.screen = other;
                return;
            }
        };
        let root = b.tree.root_path().to_path_buf();
        self.screen = match scan(&root, self.policy.euid) {
            Ok(s) => {
                let saved = b.save();
                drop(b);
                sys::give_back_free_memory();
                // The last drawing's tiles and locks were that tree's NodeIds.
                self.forget_hits();
                self.progress_drawn = Instant::now();
                self.scan_started = self.progress_drawn;
                Screen::Refreshing(s, saved)
            }
            Err(e) => {
                let what = format!("scan {}: {e}", root.display());
                self.message = Some(match self.message.take() {
                    Some(m) => format!("{m}; cannot {what}"),
                    None => format!("Cannot {what}"),
                });
                Screen::Browse(b)
            }
        };
    }

    /// A new tree replaces the last: what the last drawing found where (tiles, locks, tick
    /// boxes: the last tree's NodeIds) means nothing until the next drawing.
    fn forget_hits(&mut self) {
        self.hits.tiles.clear();
        self.hits.locks.clear();
        self.hits.boxes.clear();
        self.hover = Hover::Nothing;
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
                self.forget_hits();
                if mem::take(&mut self.empty_next) && matches!(self.screen, Screen::Browse(_)) {
                    self.ask_empty(0);
                }
            }
            return true;
        }
        if let Screen::Browse(b) = &mut self.screen {
            if let Some(mut e) = b.emptying.take_if(|e| e.finished()) {
                self.message = Some(emptied(e.job.take().map(JoinHandle::join)));
                self.done = true;
                // Then from the disk, as for a move to the trash.
                self.refresh();
                return true;
            }
            if b.emptying.is_some() && self.progress_drawn.elapsed() >= PROGRESS {
                self.progress_drawn = Instant::now();
                return true;
            }
        }
        if matches!(&self.screen, Screen::Refreshing(s, _) if s.is_finished()) {
            if let Screen::Refreshing(s, saved) = mem::replace(&mut self.screen, Screen::Gone) {
                self.screen = match s.join() {
                    Some(tree) => {
                        let mut b = self.browse(tree);
                        b.restore(&saved);
                        Screen::Browse(Box::new(b))
                    }
                    // Stopped: the last tree is gone, so the places, as after a first scan.
                    None => Screen::Start(self.start()),
                };
                self.forget_hits();
            }
            return true;
        }
        match &mut self.screen {
            Screen::Scanning(_) | Screen::Refreshing(..)
                if self.progress_drawn.elapsed() >= PROGRESS =>
            {
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
            Screen::Scanning(_) | Screen::Refreshing(..) => Some(SCAN_CHECK),
            Screen::Browse(b) if b.emptying.is_some() => Some(SCAN_CHECK),
            Screen::Browse(b) => {
                let arm = b.confirm.as_ref().filter(|c| !c.shown_armed);
                arm.map(|c| ARM.saturating_sub(c.opened.elapsed()))
            }
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
            ticked: HashSet::new(),
            drag: None,
            emptying: None,
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
        // Scanning again: Esc stops it, `q` quits; any other key waits, and leaves the message
        // (what the trash did) on screen.
        if let Screen::Refreshing(s, _) = &self.screen {
            return match key.code {
                KeyCode::Esc => {
                    s.cancel();
                    Outcome::None
                }
                KeyCode::Char('q') => Outcome::Quit,
                _ => Outcome::None,
            };
        }
        // Emptying the trash: Esc stops it (the item it is at stays in the trash, partly
        // deleted), `q` quits; any other key waits.
        if let Screen::Browse(b) = &self.screen {
            if let Some(e) = &b.emptying {
                return match key.code {
                    KeyCode::Esc => {
                        e.tally.stop.store(true, Relaxed);
                        Outcome::Redraw
                    }
                    KeyCode::Char('q') => Outcome::Quit,
                    _ => Outcome::None,
                };
            }
        }
        // A key ends a drag (the button held meanwhile): what it does may change the rows or
        // the ticks the drag started from.
        if let Screen::Browse(b) = &mut self.screen {
            b.drag = None;
        }
        let had_message = self.message.take().is_some();
        self.done = false;
        let mode = match &self.screen {
            Screen::Start(_) => Mode::Start,
            Screen::Scanning(_) => Mode::Scanning,
            Screen::Browse(b) if b.confirm.is_some() => Mode::Confirm,
            Screen::Browse(_) => Mode::Browse,
            Screen::Refreshing(..) | Screen::Gone => Mode::Gone,
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
            KeyCode::Delete => return self.empty_from_start(),
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('q') | KeyCode::Esc => return Outcome::Quit,
            _ => return Outcome::None,
        }
        Outcome::Redraw
    }

    /// Del on the start screen's Trash: the trash is scanned (its size, a card meanwhile, Esc
    /// stops it), then the dialog to empty it opens over its tree. Not when read-only.
    fn empty_from_start(&mut self) -> Outcome {
        let Screen::Start(s) = &self.screen else {
            return Outcome::None;
        };
        let place = &s.places[s.sel];
        let trash = self.policy.trash_dir();
        if self.policy.read_only().is_some() || place.path != trash {
            return Outcome::None;
        }
        if !place.exists {
            self.message = Some("The trash is empty: there is none yet".into());
            return Outcome::Redraw;
        }
        self.start_scan(&trash);
        self.empty_next = matches!(self.screen, Screen::Scanning(_));
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
                if b.open_folder(sel) {
                    b.rebuild(Some(sel));
                } else if b.shows_open(sel) && b.cursor + 1 < b.rows.len() {
                    b.cursor += 1;
                }
            }
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Char('-') => {
                if b.close_folder(sel) {
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
            KeyCode::Char('r') => self.refresh(),
            KeyCode::Char('o') => return Outcome::Open(b.tree.path(sel)),
            KeyCode::Char('c') => {
                self.message = Some(format!("Copied {}", b.tree.path(sel).display()));
                return Outcome::CopyPath(b.tree.path(sel));
            }
            KeyCode::Delete if !read_only => self.ask_trash(),
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('q') => return Outcome::Quit,
            // A tick on the row, or off it (not the shown folder's), and on to the next row.
            KeyCode::Char(' ') => {
                if sel != b.zoom {
                    b.flip_tick(sel);
                }
                b.cursor = (b.cursor + 1).min(b.rows.len() - 1);
            }
            KeyCode::Esc => b.ticked.clear(),
            _ => return Outcome::None,
        }
        Outcome::Redraw
    }

    /// Opens the confirmation dialog for the selected item, or says why it cannot be trashed.
    fn ask_trash(&mut self) {
        let Screen::Browse(b) = &mut self.screen else {
            return;
        };
        if !b.ticked.is_empty() {
            return self.ask_trash_ticked();
        }
        let id = b.rows[b.cursor].0;
        if b.tree.path(id) == self.policy.trash_dir() {
            return self.ask_empty(id);
        }
        // ponytail: `check` here and `trash` on confirm each walk the whole subtree on the
        // thread that draws: 10-20 ms for 10 000 entries warm, seconds for a huge folder read
        // cold. When that shows, run them on a worker thread (`trash` takes `&Tree` for it).
        if let Err(r) = guard::check(&b.tree, id, &self.policy) {
            // A folder with no lock of its own: how many locked items the scan saw in it (the
            // reason is still the guard's, from the disk).
            let n = b.tree.node(id);
            let held = match n.flags & (LOCKED | HOLDS) == HOLDS {
                true => locked_inside(&b.tree, id),
                false => 0,
            };
            let about_them = matches!(
                r,
                Refusal::Protected(_)
                    | Refusal::NotOwned(_)
                    | Refusal::Mount(_)
                    | Refusal::Unreadable(_)
            );
            self.message = Some(match held {
                1.. if about_them => {
                    let s = if held == 1 { "" } else { "s" };
                    format!("Cannot move to the trash: it holds {held} locked item{s}; {r}")
                }
                _ => format!("Cannot move to the trash: {r}"),
            });
            return;
        }
        b.confirm = Some(Confirm::new(vec![(id, b.tree.path(id))], Vec::new()));
    }

    /// The dialog for the ticked items (not the cursor's row), in path order, each checked by
    /// the guard: what shows ticked is what may go. A ticked folder it passes goes whole. One
    /// it refuses is listed with why and stays; if what is in it may still go (`opens`), each
    /// item in it is asked on its own, and so on down. If it refuses them all, a message and
    /// no dialog.
    fn ask_trash_ticked(&mut self) {
        let Screen::Browse(b) = &mut self.screen else {
            return;
        };
        let tree = &b.tree;
        let mut work: Vec<NodeId> = b
            .ticked
            .iter()
            .copied()
            .filter(|&id| !gone(tree, id))
            .collect();
        let (mut items, mut refused) = (Vec::new(), Vec::new());
        while let Some(id) = work.pop() {
            let path = tree.path(id);
            match guard::check(tree, id, &self.policy) {
                Ok(()) => items.push((id, path)),
                Err(r) => {
                    if tree.node(id).is_dir() && opens(&r, &path, &self.policy) {
                        work.extend(tree.children(id).filter(|&c| !gone(tree, c)));
                    }
                    refused.push((path, r.to_string()));
                }
            }
        }
        items.sort_by(|a, b| a.1.cmp(&b.1));
        refused.sort_by(|a, b| a.0.cmp(&b.0));
        if items.is_empty() {
            let first = refused.first().map_or("", |r| r.1.as_str());
            self.message = Some(match refused.len() {
                1 => format!("Cannot move to the trash: the ticked item cannot go; {first}"),
                n => format!(
                    "Cannot move to the trash: none of the {n} ticked items can go; {first}"
                ),
            });
            return;
        }
        let mut c = Confirm::new(items, refused);
        c.ticked = true;
        b.confirm = Some(c);
    }

    /// The dialog to empty the trash, whose folder is `id`: how many items it holds and their
    /// size, from the scan. If it holds nothing, or the guard says it cannot be emptied (the
    /// checks make nothing), a message instead.
    fn ask_empty(&mut self, id: NodeId) {
        let Screen::Browse(b) = &mut self.screen else {
            return;
        };
        if trash_items(&b.tree, id) == 0 {
            self.message = Some("The trash is empty".into());
            return;
        }
        if let Err(r) = guard::empty_trash(&self.policy) {
            self.message = Some(format!("Cannot empty the trash: {r}"));
            return;
        }
        let mut c = Confirm::new(vec![(id, b.tree.path(id))], Vec::new());
        c.empty = true;
        b.confirm = Some(c);
    }

    /// Empties the trash on a thread of its own, after the guard checks it again; the tree
    /// waits under a card meanwhile (`tick` picks up the end).
    fn start_empty(&mut self) {
        let Screen::Browse(b) = &mut self.screen else {
            return;
        };
        let Some(c) = b.confirm.take() else {
            return;
        };
        let job = match guard::empty_trash(&self.policy) {
            Ok(job) => job,
            Err(r) => {
                self.message = Some(format!("Cannot empty the trash: {r}"));
                return;
            }
        };
        let tally = Arc::new(Tally::default());
        let shared = Arc::clone(&tally);
        b.emptying = Some(Emptying {
            tally,
            of: trash_items(&b.tree, c.items[0].0),
            job: Some(thread::spawn(move || job.run(&shared))),
        });
        self.progress_drawn = Instant::now();
        self.scan_started = self.progress_drawn;
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
            KeyCode::Enter if c.may_move() && c.empty => self.start_empty(),
            KeyCode::Enter if c.may_move() => self.do_trash(),
            // At once: only Enter waits (`may_move`).
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Left | KeyCode::Right => {
                c.on_move = !c.on_move;
                c.focused = Instant::now();
            }
            // The list of several paths (drawing keeps it in range).
            KeyCode::Up => c.scroll = c.scroll.saturating_sub(1),
            KeyCode::Down => c.scroll = c.scroll.saturating_add(1),
            KeyCode::PageUp => c.scroll = c.scroll.saturating_sub(c.page),
            KeyCode::PageDown => c.scroll = c.next_page.max(c.scroll.saturating_add(1)),
            KeyCode::Home => c.scroll = 0,
            KeyCode::End => c.scroll = usize::MAX,
            _ => return Outcome::None,
        }
        Outcome::Redraw
    }

    /// Moves the items in the dialog to the trash, one at a time, each through the guard,
    /// which checks it all again. What it refuses stays (ticked, if it was).
    fn do_trash(&mut self) {
        let Screen::Browse(b) = &mut self.screen else {
            return;
        };
        let Some(c) = b.confirm.take() else {
            return;
        };
        let (mut moved, mut bytes, mut refused) = (0, 0, Vec::new());
        let mut what = String::new();
        for &(id, _) in &c.items {
            let (n, items) = (*b.tree.node(id), b.tree.items(id));
            let size = if self.apparent { n.apparent } else { n.alloc };
            match guard::trash(&b.tree, id, &self.policy) {
                Ok(_) => {
                    b.tree.mark_trashed(id);
                    (moved, bytes) = (moved + 1, bytes + size);
                    let size = crate::fmt_size(size);
                    let name = crate::draw::shown(b.tree.name(id));
                    what = match n.is_dir() {
                        true => format!("{name}, {}, {size}", count(items.into(), "item")),
                        false => format!("{name}, {size}"),
                    };
                }
                Err(r) => refused.push(r),
            }
        }
        // Shown until the next key or click (no timer); that key or click also does its own
        // work, as with every message.
        let freed = "freed when the trash is emptied";
        let mut msg = match (c.one(), moved) {
            (true, 1) => format!("Moved to the trash: {what} - {freed}"),
            (_, 0) => String::new(),
            _ => format!(
                "Moved {} to the trash, {} - {freed}",
                count(moved, "item"),
                crate::fmt_size(bytes)
            ),
        };
        if let Some(r) = refused.first() {
            msg = match (moved, c.one()) {
                (0, true) => format!("Not moved: {r}"),
                (0, false) => format!("None moved: {} refused; {r}", refused.len()),
                _ => format!("{msg}; {} not moved: {r}", refused.len()),
            };
        }
        self.message = Some(msg);
        if moved == 0 {
            return;
        }
        self.done = true;
        let tree = &b.tree;
        b.ticked.retain(|&id| !gone(tree, id));
        b.sorted.clear();
        b.ordered.clear();
        while b.zoom != 0 && gone(&b.tree, b.zoom) {
            b.zoom = b.tree.parent(b.zoom).unwrap_or(0);
        }
        let at = b.cursor;
        b.rebuild(None);
        b.cursor = at.min(b.rows.len() - 1);
        // Then from the disk, as it is now: what this tree shows as the trash left it (the row
        // after the one that went selected) is kept by path, and the new tree shows it so.
        self.refresh();
    }

    /// A mouse event: give the view every one, the left button's drags and releases too (a
    /// drag over the tree's rows ticks them; the CLI's terminal reports them with any-event
    /// tracking, as crossterm's `EnableMouseCapture` turns on).
    pub fn handle_mouse(&mut self, m: MouseEvent) -> Outcome {
        let (x, y) = (m.column, m.row);
        let inside = |r: Rect| x >= r.x && x < r.right() && y >= r.y && y < r.bottom();
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Screen::Browse(b) = &mut self.screen {
                    b.drag = None; // a row below may start one again
                }
            }
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
                    // Over the dialog: its list of paths (drawing keeps it in range).
                    if let Some(c) = &mut b.confirm {
                        c.scroll = match up {
                            true => c.scroll.saturating_sub(3),
                            false => c.scroll.saturating_add(3),
                        };
                        return Outcome::Redraw;
                    }
                    if tree {
                        b.cursor = match up {
                            true => b.cursor.saturating_sub(3),
                            false => (b.cursor + 3).min(b.rows.len() - 1),
                        };
                        return Outcome::Redraw;
                    }
                }
                return Outcome::None;
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.pointer = Some((x, y));
                return self.drag_to(y);
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let Screen::Browse(b) = &mut self.screen {
                    b.drag = None;
                }
                return Outcome::None;
            }
            _ => return Outcome::None,
        }
        let now = Instant::now();
        let double = self
            .last_click
            .is_some_and(|(t, cx, cy)| now - t < DOUBLE && (cx, cy) == (x, y));
        // A row's tick box: each click there ticks or unticks, none is part of a double click.
        let tick_box = self.hits.boxes.iter().find(|b| inside(b.0)).map(|b| b.1);
        // A double click is used up: a third quick click starts over as a single one.
        self.last_click = match double || tick_box.is_some() {
            true => None,
            false => Some((now, x, y)),
        };
        if self.help {
            self.help = false;
            return Outcome::Redraw;
        }
        if self.hits.close.is_some_and(inside) {
            // Quits, as `q` does; under the dialog, nothing (and it shows no hover there).
            return match self.close_works() {
                true => Outcome::Quit,
                false => Outcome::None,
            };
        }
        let emptying = matches!(&self.screen, Screen::Browse(b) if b.emptying.is_some());
        if matches!(self.screen, Screen::Refreshing(..)) || emptying {
            return Outcome::None; // waits, as keys do, and leaves the message
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
                let empty = b.confirm.as_ref().is_some_and(|c| c.empty);
                if inside(cancel) {
                    b.confirm = None;
                } else if inside(moving) && ok && empty {
                    self.start_empty();
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
            Screen::Browse(b) if tick_box.is_some() => {
                // Ticked or not, with the cursor on its row; a drag from here gives the rows it
                // goes over the state this box took.
                let Some(id) = tick_box else {
                    return Outcome::None;
                };
                if let Some(row) = b.rows.iter().position(|r| r.0 == id) {
                    b.cursor = row;
                    let on = b.flip_tick(id);
                    b.drag = Some(Drag {
                        anchor: id,
                        before: b.ticked.clone(),
                        moved: false,
                        on,
                    });
                }
            }
            Screen::Browse(b) if inside(self.hits.rows) => {
                let row = b.top + usize::from(y - self.hits.rows.y);
                if row < b.rows.len() {
                    b.cursor = row;
                    if double {
                        b.toggle(b.rows[row].0);
                    }
                    // A drag may start here.
                    let (anchor, before) = (b.rows[b.cursor].0, b.ticked.clone());
                    b.drag = Some(Drag {
                        anchor,
                        before,
                        moved: false,
                        on: true,
                    });
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

    /// The pointer dragged with the left button to line `y`: once it has left the row the drag
    /// started on, the rows from there to the one under it are ticked (on top of what was
    /// ticked before; dragging back takes ticks off again), or unticked if it started on a box
    /// it unticked (`tick_on`, `tick_off`: a row in a ticked folder is ticked already, and
    /// unticking it splits that folder's tick); the shown folder's row never, and the cursor
    /// follows. Above or below the
    /// rows, the next row past the edge: the list scrolls as the cursor goes.
    fn drag_to(&mut self, y: u16) -> Outcome {
        let rows = self.hits.rows;
        let Screen::Browse(b) = &mut self.screen else {
            return Outcome::None;
        };
        let Some(d) = b.drag.as_mut().filter(|_| b.confirm.is_none()) else {
            return Outcome::None;
        };
        let Some(anchor) = b.rows.iter().position(|r| r.0 == d.anchor) else {
            b.drag = None; // its row is gone
            return Outcome::None;
        };
        let last = b.rows.len() - 1;
        let row = match y {
            y if y < rows.y => b.top.saturating_sub(1),
            y if y >= rows.bottom() => b.top + usize::from(rows.height),
            y => b.top + usize::from(y - rows.y),
        };
        let row = row.min(last);
        if row == anchor && !d.moved {
            return Outcome::None;
        }
        d.moved = true;
        let (on, before) = (d.on, d.before.clone());
        let range = anchor.min(row)..=anchor.max(row);
        let zoom = b.zoom;
        let rows: Vec<NodeId> = b.rows[range].iter().map(|r| r.0).collect();
        // From the top down, so a folder comes before what is in it. Off over a folder holding
        // ticks (◩) takes those off too: each row ends as the drag paints it.
        b.ticked = before;
        let part = match on {
            true => HashSet::new(),
            false => b.partly_ticked(),
        };
        for id in rows.into_iter().filter(|&id| id != zoom) {
            match on {
                true => b.tick_on(id),
                false if part.contains(&id) && !b.shows_ticked(id) => b.clear_below(id),
                false => b.tick_off(id),
            }
        }
        b.tidy();
        b.cursor = row;
        Outcome::Redraw
    }

    /// The close button does something (what `q` does): everywhere but under the dialog.
    pub(crate) fn close_works(&self) -> bool {
        !matches!(&self.screen, Screen::Browse(b) if b.confirm.is_some())
    }

    /// What is under (x, y) in the last drawing, for hover: the same places, in the same order,
    /// as a click looks for them in `handle_mouse`.
    pub(crate) fn hover_at(&self, x: u16, y: u16) -> Hover {
        let inside = |r: Rect| x >= r.x && x < r.right() && y >= r.y && y < r.bottom();
        let h = &self.hits;
        if self.help {
            return Hover::Nothing;
        }
        if h.close.is_some_and(inside) && self.close_works() {
            return Hover::Close;
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
            // A lock lies on its row or tile: looked for first, the click goes to those.
            Screen::Browse(_) if h.locks.iter().any(|l| inside(l.0)) => {
                Hover::Lock(h.locks.iter().find(|l| inside(l.0)).map_or(0, |l| l.1))
            }
            Screen::Browse(_) if h.boxes.iter().any(|b| inside(b.0)) => {
                Hover::Box(h.boxes.iter().find(|b| inside(b.0)).map_or(0, |b| b.1))
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

/// Whether the guard's refusal `r` of the folder at `path` leaves what is in it to be asked on
/// its own: the refusal names something inside it, or the folder protects only itself
/// (`~/.cache`, Downloads). Not when the folder itself is not the user's, a mount point,
/// unreadable or a wholly protected folder or in one (its items would be refused too), nor
/// for a refusal of any move (read-only, another filesystem, no trash, changed); never a home
/// (a ticked home would tick all in it that may go).
fn opens(r: &Refusal, path: &Path, policy: &Policy) -> bool {
    if policy.is_home(path) || matches!(policy.protection(path), Some((_, true, _))) {
        return false;
    }
    match r {
        Refusal::Protected(p)
        | Refusal::NotOwned(p)
        | Refusal::Mount(p)
        | Refusal::Unreadable(p)
            if p != path && p.starts_with(path) =>
        {
            true
        }
        Refusal::Protected(_) => matches!(policy.protection(path), Some((_, false, true))),
        _ => false,
    }
}

/// How many items the trash whose folder is `id` holds: what is in its `files`, as scanned.
pub(crate) fn trash_items(tree: &Tree, id: NodeId) -> usize {
    let mut kids = tree.children(id);
    let files = kids.find(|&c| tree.name(c) == "files" && tree.node(c).is_dir());
    files.map_or(0, |f| tree.children(f).filter(|&c| !gone(tree, c)).count())
}

/// The status bar's message for what emptying the trash did.
fn emptied(done: Option<thread::Result<Emptied>>) -> String {
    let Some(Ok(e)) = done else {
        return "Emptying the trash stopped: it failed; what is left is still in the trash".into();
    };
    let freed = crate::fmt_size(e.bytes);
    let items = count(e.items, "item");
    let mut msg = match e.stopped {
        false => format!("Emptied the trash: {items} deleted, {freed} freed"),
        true => {
            format!("Stopped emptying the trash: {items} deleted, {freed} freed; the rest stays")
        }
    };
    if let Some((path, why)) = e.failed.first() {
        let n = e.failed.len();
        msg.push_str(&format!("; {n} not deleted: {}: {why}", path.display()));
    }
    msg
}

/// Whether `id` went to the trash, itself or with a folder above it.
fn gone(tree: &Tree, id: NodeId) -> bool {
    let mut up = Some(id);
    while let Some(i) = up {
        if tree.node(i).flags & TRASHED != 0 {
            return true;
        }
        up = tree.parent(i);
    }
    false
}

/// How many locked items the scan saw under `id`, counting the top one of a locked subtree
/// once (only folders that hold one are looked into).
fn locked_inside(tree: &Tree, id: NodeId) -> usize {
    let (mut count, mut stack) = (0, vec![id]);
    while let Some(i) = stack.pop() {
        for k in tree.children(i) {
            let flags = tree.node(k).flags;
            if flags & TRASHED != 0 {
                continue;
            }
            if flags & LOCKED != 0 {
                count += 1;
            } else if flags & HOLDS != 0 {
                stack.push(k);
            }
        }
    }
    count
}

/// Why `id` itself cannot be trashed, as the scan saw it, for the box over its lock: each of its
/// locks, in the order the guard checks them. Only the tree's flags and the policy's paths: it
/// runs while drawing, so it never asks the disk or the user database (a hung mount or a slow
/// NSS would freeze the view).
pub(crate) fn lock_reason(tree: &Tree, id: NodeId, policy: &Policy) -> String {
    let flags = tree.node(id).flags;
    let path = tree.path(id);
    // A path as shown, under the home as `~/...`.
    let show = |p: &Path| match p.strip_prefix(policy.home()) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", crate::draw::shown(rest.as_os_str())),
        Err(_) => crate::draw::shown(p.as_os_str()),
    };
    let mut why = Vec::new();
    if flags & PROTECTED != 0 {
        why.push(match policy.protection(&path) {
            Some((p, true, true)) => format!("protected: {}, with everything in it", show(p)),
            Some((p, false, _)) => format!("protected: {} (what is in it may go)", show(p)),
            Some((p, true, false)) => format!("protected: inside {}", show(p)),
            None => "protected".to_string(),
        });
    }
    if flags & MOUNT != 0 {
        why.push(format!("a mount point: {}", show(&path)));
    }
    if flags & NOT_OWNED != 0 {
        why.push("not yours: another user owns it".to_string());
    }
    if flags & UNREADABLE != 0 {
        why.push("could not be read".to_string());
    }
    why.join("; ")
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
    /// on `keep` if it is still shown. A drag going on ends: its rows are gone.
    fn rebuild(&mut self, keep: Option<NodeId>) {
        self.drag = None;
        let mut rows = Vec::new();
        let mut stack = vec![(self.zoom, 0u16)];
        while let Some((id, depth)) = stack.pop() {
            rows.push((id, depth));
            if self.open.contains(&id) || id == self.zoom {
                let kids = self.ordered(id).into_iter().rev();
                stack.extend(kids.map(|k| (k, depth + 1)));
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
        if !self.close_folder(id) {
            self.open_folder(id);
        }
        self.rebuild(Some(id));
    }

    /// Opens the folder `id` if it is closed; whether it was. Lays nothing out.
    fn open_folder(&mut self, id: NodeId) -> bool {
        self.tree.node(id).is_dir() && self.open.insert(id)
    }

    /// Closes the folder `id` if it is open; whether it was. The shown folder stays open. Lays
    /// nothing out.
    fn close_folder(&mut self, id: NodeId) -> bool {
        id != self.zoom && self.open.remove(&id)
    }

    /// Puts the cursor on `id`, opening the folders between the shown one and it.
    pub fn select(&mut self, id: NodeId) {
        let mut up = self.tree.parent(id).filter(|_| id != self.zoom);
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

    /// Whether a folder above `id` is ticked (`id` goes with it).
    fn ticked_above(&self, id: NodeId) -> bool {
        let mut up = self.tree.parent(id);
        while let Some(p) = up {
            if self.ticked.contains(&p) {
                return true;
            }
            up = self.tree.parent(p);
        }
        false
    }

    /// Whether the row of `id` shows ticked: it is, or a folder above it is (it goes with it).
    pub fn shows_ticked(&self, id: NodeId) -> bool {
        self.ticked.contains(&id) || self.ticked_above(id)
    }

    /// Ticks `id` (Space, its box) or takes the tick off; returns whether it shows ticked now.
    fn flip_tick(&mut self, id: NodeId) -> bool {
        let on = !self.shows_ticked(id);
        match on {
            true => self.tick_on(id),
            false => self.tick_off(id),
        }
        self.tidy();
        on
    }

    /// Ticks `id`, with everything in it; nothing changes in a ticked folder. `tidy` then drops
    /// the ticks it holds.
    fn tick_on(&mut self, id: NodeId) {
        if !self.shows_ticked(id) {
            self.ticked.insert(id);
        }
    }

    /// Takes the tick off `id` and everything in it. In a ticked folder, that folder's tick is
    /// split: it goes, and each other item on the way down to `id` is ticked on its own (not
    /// what went to the trash), so all but `id` stays ticked.
    fn tick_off(&mut self, id: NodeId) {
        if self.ticked.remove(&id) {
            return;
        }
        let mut path = vec![id];
        let mut up = self.tree.parent(id);
        while let Some(p) = up {
            if self.ticked.remove(&p) {
                // ponytail: a tick per item beside the path: unticking one of 200 000 files in
                // a ticked folder makes 199 999 ticks. Keep "all but these" if that shows.
                let mut parent = p;
                for &next in path.iter().rev() {
                    for c in self.tree.children(parent) {
                        if c != next && !gone(&self.tree, c) {
                            self.ticked.insert(c);
                        }
                    }
                    parent = next;
                }
                return;
            }
            path.push(p);
            up = self.tree.parent(p);
        }
    }

    /// Takes off the ticks inside the folder `id` (a drag that takes ticks off, over a folder
    /// holding some: it ends with none).
    fn clear_below(&mut self, id: NodeId) {
        let lies_in = |t: NodeId| {
            let mut up = self.tree.parent(t);
            while let Some(p) = up.filter(|&p| p != id) {
                up = self.tree.parent(p);
            }
            up.is_some()
        };
        let below: Vec<NodeId> = self
            .ticked
            .iter()
            .copied()
            .filter(|&t| lies_in(t))
            .collect();
        for t in below {
            self.ticked.remove(&t);
        }
    }

    /// Drops the ticks inside a ticked folder (they go with it), so `ticked` holds only the top
    /// ones: what its count says, and what Del lists.
    fn tidy(&mut self) {
        let inside: Vec<NodeId> = (self.ticked.iter().copied())
            .filter(|&id| self.ticked_above(id))
            .collect();
        for id in inside {
            self.ticked.remove(&id);
        }
    }

    /// The folders holding a ticked item, drawn ◩ where not ticked themselves: each ticked
    /// item's folders above it, up to one met already.
    pub fn partly_ticked(&self) -> HashSet<NodeId> {
        let mut part = HashSet::new();
        for &id in &self.ticked {
            let mut up = self.tree.parent(id);
            while let Some(p) = up.filter(|&p| part.insert(p)) {
                up = self.tree.parent(p);
            }
        }
        part
    }

    /// What the ticked items add up to in this mode (`ticked` holds only the top ones).
    pub fn ticked_size(&self, apparent: bool) -> u64 {
        let ticked = self.ticked.iter();
        ticked
            .map(|&id| counted(self.tree.node(id), apparent))
            .sum()
    }

    /// Whether the row of `id` shows its folder open (the chevron).
    pub fn shows_open(&self, id: NodeId) -> bool {
        id == self.zoom || self.open.contains(&id)
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

    /// What this shows, by path below the root, for `restore` on the next scan of the folder.
    /// Paths below the root: the folder may be reached by another path next time (a symlink put
    /// where it was), so its absolute path may differ.
    fn save(&self) -> Saved {
        let tree = &self.tree;
        let below = |id: NodeId| {
            let path = tree.path(id);
            path.strip_prefix(tree.root_path())
                .map_or_else(|_| PathBuf::new(), Path::to_path_buf)
        };
        let all = |ids: &HashSet<NodeId>| ids.iter().map(|&id| below(id)).collect();
        Saved {
            root: tree.root_path().to_path_buf(),
            zoom: below(self.zoom),
            selected: below(self.rows[self.cursor].0),
            line: self.cursor.saturating_sub(self.top),
            open: all(&self.open),
            ticked: all(&self.ticked),
        }
    }

    /// Shows what the previous scan of the same folder showed (`save`): the same folder, the
    /// same folders open, the same item selected on the same line. Where one
    /// is gone, the nearest folder above it that is still there.
    fn restore(&mut self, saved: &Saved) {
        let tree = &self.tree;
        let here = |path: &PathBuf| tree.find(path);
        // The empty path, the root, is always there.
        let nearest = |path: &Path| path.ancestors().find_map(|p| tree.find(p)).unwrap_or(0);
        let is_dir = |id: &NodeId| tree.node(*id).is_dir();
        // A shown folder that is a file now: the folder holding it.
        let mut zoom = nearest(&saved.zoom);
        while zoom != 0 && !is_dir(&zoom) {
            zoom = tree.parent(zoom).unwrap_or(0);
        }
        let selected = nearest(&saved.selected);
        let open = saved.open.iter().filter_map(here).filter(is_dir);
        let open: HashSet<NodeId> = open.chain([0, zoom]).collect();
        // Ticked items still there, at the same path (not a folder above one that went). Ticks
        // pick what Del moves, so where the folder itself was swapped (a symlink put where it
        // was), a path below it may now be something else: none stay.
        let ticked: HashSet<NodeId> = match tree.root_path() == saved.root {
            true => saved.ticked.iter().filter_map(here).collect(),
            false => HashSet::new(),
        };
        (self.ticked, self.open, self.zoom) = (ticked, open, zoom);
        self.tidy();
        self.rebuild(Some(selected));
        self.top = self.cursor.saturating_sub(saved.line);
    }

    /// The treemap's folder and its selected tile: the folder that holds the selected row, and
    /// that row's item, open or closed (a folder is seen among the ones beside it); on the
    /// shown folder's own row, that folder and no tile.
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
                Field::Items => tree.items(a).cmp(&tree.items(b)), // a file has none
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

    /// The tree on screen.
    fn browse(v: &mut View) -> &mut Browse {
        match &mut v.screen {
            Screen::Browse(b) => b,
            _ => panic!("not browsing"),
        }
    }

    /// What the last tree showed, kept by path while its folder is scanned again.
    fn saved(v: &View) -> &Saved {
        match &v.screen {
            Screen::Refreshing(_, saved) => saved,
            _ => panic!("not scanning again"),
        }
    }

    /// Ticks until a scan again (after a trash, or `r`) has shown its tree.
    fn settle(v: &mut View) {
        for _ in 0..5000 {
            v.tick();
            if matches!(v.screen, Screen::Browse(_)) {
                return;
            }
            sleep(Duration::from_millis(1));
        }
        panic!("the scan again did not finish");
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
        assert!(has(&lines, "▸ ☐ big/") && has(&lines, "☐ small"));
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
    fn rule7_tab_moves_the_focus_at_once_and_enter_waits_half_a_second() {
        use ratatui::style::Modifier;
        let (_t, home) = home("view-arm");
        let mut v = ready(policy(&home, None), &home.join(".cache"));
        let before = browse(&mut v).tree.node(0).alloc;
        key(&mut v, KeyCode::Delete);
        assert!(
            v.next_wake().is_some_and(|d| d <= ARM),
            "wakes to arm the dialog"
        );
        key(&mut v, KeyCode::Tab); // at once
        assert!(browse(&mut v).confirm.as_ref().unwrap().on_move);
        // Drawn with the focus (bold, Cancel not), and crossed out: it cannot move yet.
        let buf = draw_buf(&mut v, 120, 40);
        let (cancel, moving) = v.hits.buttons.unwrap();
        assert!(mods(&buf, moving).contains(Modifier::BOLD | Modifier::CROSSED_OUT));
        assert!(!mods(&buf, cancel).contains(Modifier::BOLD));
        key(&mut v, KeyCode::Enter); // too soon: nothing, and the dialog stays
        assert!(browse(&mut v).confirm.is_some() && home.join(".cache/big").exists());
        // Del, Tab, a wait, Enter: moved.
        age(&mut v);
        assert!(v.tick(), "draws the armed dialog once");
        assert_eq!(v.next_wake(), None);
        key(&mut v, KeyCode::Enter);
        let trash = home.join(".local/share/Trash");
        assert!(!home.join(".cache/big").exists());
        assert!(trash.join("files/big/inner/f19").exists());
        assert!(trash.join("info/big.trashinfo").exists());
        // The scan's card meanwhile, with the message; then the tree from the disk, without it.
        assert!(has(&text(&mut v, 120, 40), "Moved to the trash"));
        settle(&mut v);
        let lines = text(&mut v, 120, 40);
        assert!(has(&lines, "Moved to the trash"), "until a key or a click");
        assert!(!has(&lines, "big/"), "gone from the tree and the map");
        assert!(browse(&mut v).tree.node(0).alloc < before, "sizes updated");
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
        // The pointer on a lock: its reason in a box.
        let mut tip = ready(policy(home, None), home);
        text(&mut tip, 120, 40);
        let lock = tip.hits.locks[0].0;
        hover(&mut tip, lock.x + 1, lock.y);
        assert!(matches!(tip.hover, Hover::Lock(_)));
        views.push(tip);
        // Ticks and the status line; the dialog for them (a path, a refused one).
        for dialog in [false, true] {
            let mut ticks = ready(policy(home, None), home);
            let small = browse(&mut ticks).tree.find(&cache.join("small")).unwrap();
            browse(&mut ticks).select(small);
            key(&mut ticks, KeyCode::Char(' '));
            select(&mut ticks, ".config");
            key(&mut ticks, KeyCode::Char(' '));
            if dialog {
                key(&mut ticks, KeyCode::Delete);
                assert!(browse(&mut ticks).confirm.is_some());
            }
            views.push(ticks);
        }
        views
    }

    fn mouse(v: &mut View, kind: MouseEventKind, x: u16, y: u16) -> Outcome {
        v.handle_mouse(MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        })
    }

    /// The names of the ticked items, A to Z.
    fn ticked(v: &mut View) -> Vec<String> {
        let b = browse(v);
        let names = b.ticked.iter().map(|&i| b.tree.name(i).to_string_lossy());
        let mut names: Vec<String> = names.map(|n| n.into_owned()).collect();
        names.sort();
        names
    }

    /// Ticks the item at `path` (Space on its row).
    fn tick(v: &mut View, path: &Path) {
        let id = browse(v).tree.find(path).unwrap();
        browse(v).select(id);
        key(v, KeyCode::Char(' '));
    }

    /// Arms the open dialog and moves the focus to Move to Trash, as time would, without
    /// drawing it.
    fn arm(v: &mut View) {
        let c = browse(v).confirm.as_mut().unwrap();
        c.opened -= Duration::from_secs(1);
        c.shown_armed = true;
        key(v, KeyCode::Tab);
        browse(v).confirm.as_mut().unwrap().focused -= Duration::from_secs(1);
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
        // Every cell of the selected tile's outline selects that tile, in every class mode. A
        // file big enough for a tile of its own beside it (small and linked share the rest).
        fs::write(cache.join("wide"), vec![0u8; 200_000]).unwrap();
        for t in [false, true] {
            let mut v = ready(policy(&home, None), &cache);
            v.by_age = t;
            select(&mut v, "big");
            let buf = draw_buf(&mut v, 120, 40);
            let b = browse(&mut v);
            let big = b.rows[b.cursor].0;
            let tile = v.hits.tiles.iter().find(|t| t.1 == Some(big)).unwrap().0;
            assert_eq!(
                buf[(tile.x, tile.y)].symbol(),
                "▌",
                "the selected tile's mark"
            );
            // The frame's right edge is left of the gap column.
            assert_eq!(buf[(tile.right() - 2, tile.y)].symbol(), "╮", "the outline");
            // The other tiles are framed too.
            let wide = browse(&mut v).tree.find(&cache.join("wide")).unwrap();
            let other = v.hits.tiles.iter().find(|t| t.1 == Some(wide)).unwrap().0;
            assert_eq!(
                buf[(other.x, other.y)].symbol(),
                "╭",
                "every tile is framed"
            );
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
        // An "other" with a tile of its own (small and linked share the rest).
        fs::write(cache.join("data.bin"), vec![0u8; 50_000]).unwrap();
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

    /// The text in `r`'s cells, a line a row.
    fn text_in(lines: &[String], r: Rect) -> Vec<String> {
        (r.y..r.bottom())
            .map(|y| (r.x..r.right()).map(|x| text_at(lines, x, y)).collect())
            .collect()
    }

    /// A folder of one big file and 300 small ones ("s000" of 100 bytes to "s299" of 399), in
    /// apparent sizes: most of the small ones share the rest.
    fn many_small(tag: &str) -> (TempDir, View) {
        let (t, home) = home(tag);
        let many = home.join(".cache/many");
        fs::create_dir(&many).unwrap();
        fs::write(many.join("big"), vec![0u8; 700_000]).unwrap();
        for i in 0..300 {
            fs::write(many.join(format!("s{i:03}")), vec![0u8; 100 + i]).unwrap();
        }
        let mut v = ready(policy(&home, None), &many);
        key(&mut v, KeyCode::Char('a')); // apparent sizes: the lengths, whatever the disk does
        v.panes = Panes::Map;
        (t, v)
    }

    #[test]
    fn the_rest_lists_what_it_holds_largest_first_and_how_many_more() {
        let (_t, mut v) = many_small("view-rest-list");
        // Where the map gives the rest room for lines (it may also end a strip, narrow).
        let roomy = [(60, 30), (80, 30), (100, 40), (120, 40)]
            .into_iter()
            .find(|&(w, h)| {
                text(&mut v, w, h);
                let rest = v.hits.tiles.iter().find(|t| t.1.is_none()).unwrap().0;
                rest.width >= 30 && rest.height >= 6
            });
        let (w, h) = roomy.expect("a rest wide and high enough for lines at one size at least");
        let lines = text(&mut v, w, h);
        let rest = v.hits.tiles.iter().find(|t| t.1.is_none()).unwrap().0;
        // On its top edge the label, whole; inside its frame (in a cell from each edge, the gap
        // column and the gutter row outside it) a line for each item, whole, the last saying
        // how many more.
        let own = v.hits.tiles.iter().filter(|t| t.1.is_some()).count();
        let count = 301 - own;
        let top = text_in(&lines, Rect::new(rest.x + 1, rest.y, rest.width - 3, 1));
        assert!(top[0].starts_with(&format!("+{count} items")), "{top:?}");
        let inside = Rect::new(rest.x + 1, rest.y + 1, rest.width - 3, rest.height - 3);
        let inside: Vec<String> = text_in(&lines, inside)
            .iter()
            .map(|l| l.trim_end().to_string())
            .collect();
        let (more, listed) = inside.split_last().unwrap();
        assert!(listed.len() >= 3, "{inside:#?}");
        // Largest first: s299 is the largest of them, so each line's number is lower.
        let numbers: Vec<u32> = listed.iter().map(|l| l[1..4].parse().unwrap()).collect();
        assert!(numbers.windows(2).all(|w| w[0] > w[1]), "{inside:#?}");
        for (line, n) in listed.iter().zip(&numbers) {
            let size = format!("{} bytes", 100 + n);
            assert!(line.contains(&size) && line.ends_with("<1%"), "{inside:#?}");
        }
        assert_eq!(*more, format!("+{} more", count - listed.len()));
    }

    #[test]
    fn the_rest_shows_its_count_whole_however_narrow() {
        let (_t, mut v) = many_small("view-rest-narrow");
        for (w, h) in [(24, 12), (30, 16), (40, 12), (60, 30), (80, 24)] {
            for chosen in [false, true] {
                if chosen {
                    select(&mut v, "s000"); // the smallest: in the rest
                }
                let lines = text(&mut v, w, h);
                let rest = v.hits.tiles.iter().find(|t| t.1.is_none()).unwrap().0;
                let own = v.hits.tiles.iter().filter(|t| t.1.is_some()).count();
                let cells = text_in(&lines, rest);
                let count = format!("+{}", 301 - own);
                assert!(
                    cells.iter().any(|l| l.contains(&count)),
                    "{w}x{h}, chosen {chosen}: {cells:?}"
                );
            }
        }
    }

    #[test]
    fn no_tile_and_no_line_of_the_rest_is_cut() {
        // Whole or not at all: no cut mark inside any tile, in either icon set.
        let (_t, home) = home("view-never-cut");
        let (_m, many) = many_small("view-never-cut-many");
        let mut views = vec![
            ready(policy(&home, None), &home),
            ready(policy(&home, None), &home.join(".cache")),
            many,
        ];
        for v in &mut views {
            for set in [IconSet::Unicode, IconSet::Ascii] {
                v.set_icons(set);
                let cut = set.glyphs().cut;
                for panes in [Panes::Both, Panes::Map] {
                    v.panes = panes;
                    for (w, h) in [(120, 40), (120, 34), (80, 24), (60, 30), (40, 12)] {
                        let lines = text(v, w, h);
                        for (r, _) in v.hits.tiles.clone() {
                            let cells = text_in(&lines, r).concat();
                            assert!(!cells.contains(cut), "{set:?} {w}x{h} {r:?}: {cells}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn under_each_tile_the_gutter_is_half_a_row_in_its_colour() {
        use crate::style::{Colors, Depth};
        let (_t, home) = home("view-gutter");
        for (set, depth) in [
            (IconSet::Unicode, Depth::True),
            (IconSet::Unicode, Depth::C16),
            (IconSet::Ascii, Depth::True),
        ] {
            let mut v = ready(policy(&home, None), &home.join(".cache/big/inner"));
            v.set_icons(set);
            v.set_colors(Colors::default().fitted(depth));
            let buf = draw_buf(&mut v, 120, 40);
            let pane = v.colors.bg;
            let tiles = v.hits.tiles.clone();
            assert_eq!(tiles.len(), 20, "inner's twenty files");
            for (r, _) in tiles {
                // The tile's colour, on its size line; the gutter is the tile's last row.
                let colour = buf[(r.x + 1, r.y + 1)].bg;
                let gutter = &buf[(r.x + 1, r.bottom() - 1)];
                let seen = (gutter.symbol(), gutter.fg, gutter.bg);
                match set {
                    IconSet::Ascii => assert_eq!((seen.0, seen.2), (" ", pane), "{r:?}"),
                    _ => assert_eq!(seen, ("▀", colour, pane), "{set:?} {depth:?} {r:?}"),
                }
                assert_ne!(colour, pane, "{r:?}");
                // The gap column at the right is the pane's, all the way down.
                assert_eq!(buf[(r.right() - 1, r.bottom() - 1)].bg, pane, "{r:?}");
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
        // The folder is scanned again, the message staying through it; then nothing waits.
        settle(&mut v);
        assert_eq!(v.message, Some(msg));
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
        settle(&mut v);
        assert!(v.done, "through the scan again");
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
        assert!(has(&lines, "▸ ☐ .cache/") && !has(&lines, "big/"));
    }

    #[test]
    fn tree_and_map_select_each_other_and_a_double_click_opens_and_closes() {
        let (_t, home) = home("view-mouse");
        // A file with a tile of its own beside big's (small and linked share the rest).
        fs::write(home.join(".cache/wide"), vec![0u8; 200_000]).unwrap();
        let mut v = ready(policy(&home, None), &home.join(".cache"));
        text(&mut v, 120, 40);
        let wide = browse(&mut v).tree.find(&home.join(".cache/wide")).unwrap();
        let tile = v.hits.tiles.iter().find(|t| t.1 == Some(wide)).unwrap().0;
        click(&mut v, tile.x, tile.y);
        assert_eq!(selected_name(&mut v), "wide", "a tile selects its row");
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
        assert!(has(&text(&mut v, 120, 40), "▾ ☐ big/"));
        assert_eq!(selected_name(&mut v), "big");
        assert_eq!(
            browse(&mut v).map_of(),
            (0, Some(big)),
            "open, still among its neighbours"
        );
        click(&mut v, x, y);
        click(&mut v, x, y);
        assert!(!browse(&mut v).open.contains(&big) && !row_shown(&mut v, "inner"));
        assert_eq!(browse(&mut v).map_of(), (0, Some(big)), "closed, the same");
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
    fn headings_underline_their_text_only_and_faint_lines_part_the_columns() {
        use ratatui::style::Modifier;
        let (_t, home) = home("view-heads");
        let mut v = ready(policy(&home, None), &home.join(".cache"));
        draw_buf(&mut v, 120, 40);
        let heads = v.hits.heads.clone();
        for (at, _) in &heads {
            v.pointer = Some((at.x, at.y));
            let buf = draw_buf(&mut v, 120, 40);
            let under: Vec<bool> = (at.x..at.right())
                .map(|x| buf[(x, at.y)].modifier.contains(Modifier::UNDERLINED))
                .collect();
            let text: Vec<bool> = (at.x..at.right())
                .map(|x| buf[(x, at.y)].symbol() != " ")
                .collect();
            assert_eq!(under, text, "only the text is underlined at {at:?}");
        }
        v.pointer = None;
        let buf = draw_buf(&mut v, 120, 40);
        let row = v.hits.rows.y;
        for (at, _) in &heads[1..] {
            for y in [at.y, row] {
                let cell = &buf[(at.x - 1, y)];
                assert_eq!(
                    (cell.symbol(), cell.fg),
                    ("│", v.colors.column_line),
                    "{at:?}"
                );
            }
        }
    }

    #[test]
    fn the_red_cross_quits_as_q_does_and_not_under_the_dialog() {
        let (_t, home) = home("view-close");
        let mut v = ready(policy(&home, None), &home.join(".cache"));
        let buf = draw_buf(&mut v, 120, 40);
        let close = v.hits.close.unwrap();
        assert_eq!(
            (close.x, close.y, close.width),
            (117, 0, 3),
            "the last 3 cells of the top"
        );
        assert_eq!(buf[(close.x + 1, close.y)].symbol(), "✕");
        assert_eq!(buf[(close.x + 1, close.y)].fg, v.colors.close_fg);
        assert_eq!(v.hover_at(close.x + 1, 0), Hover::Close);
        v.pointer = Some((close.x + 1, 0));
        let buf = draw_buf(&mut v, 120, 40);
        assert_eq!(
            buf[(close.x + 1, 0)].bg,
            v.colors.close_hover,
            "red under the pointer"
        );
        // Under the dialog `q` does nothing, and so does the cross.
        key(&mut v, KeyCode::Delete);
        assert!(browse(&mut v).confirm.is_some());
        draw_buf(&mut v, 120, 40);
        assert_eq!(v.hover_at(close.x + 1, 0), Hover::Nothing);
        assert_eq!(click(&mut v, close.x + 1, 0), Outcome::None);
        assert!(browse(&mut v).confirm.is_some(), "the dialog stays");
        key(&mut v, KeyCode::Esc);
        draw_buf(&mut v, 120, 40);
        assert_eq!(click(&mut v, close.x, 0), Outcome::Quit);
        // The start screen has it too.
        let mut v = View::new(policy(&home, None), None);
        draw_buf(&mut v, 80, 24);
        let close = v.hits.close.unwrap();
        assert_eq!(click(&mut v, close.x + 2, 0), Outcome::Quit);
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
        // 40x12: a and b get tiles, a few small files too, the others one "+N items" tile. Which
        // of the twenty (all of one size) come first is the folder's order: ask the map.
        let (all, _) = tiles(&mut v, 40, 12);
        assert!(all.starts_with(&["a".into(), "b".into()]), "{all:?}");
        assert_eq!(all.last().map(String::as_str), Some("+"), "{all:?}");
        let small_tile = all.iter().find(|n| n.starts_with('s')).cloned();
        let in_rest = (0..20)
            .map(|i| format!("s{i}"))
            .find(|n| !all.contains(n))
            .unwrap();
        let mut picks = vec![("b".to_string(), "b".to_string()), ("a".into(), "a".into())];
        picks.push((in_rest, "+".into()));
        picks.extend(small_tile.map(|n| (n.clone(), n)));
        for (name, blue) in picks {
            select(&mut v, &name);
            let (now, lit) = tiles(&mut v, 40, 12);
            assert_eq!(now, all);
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
        // Big enough that each has room for a tile of its own on any filesystem (a folder's own
        // blocks, none on tmpfs and 4 kB on ext4, change nothing here).
        fs::write(dir.join("d_big/x"), vec![0u8; 500_000]).unwrap();
        fs::write(dir.join("D_small/x"), vec![0u8; 100_000]).unwrap();
        fs::write(dir.join("D_small/y"), vec![0u8; 100_000]).unwrap();
        fs::write(dir.join("f_big"), vec![0u8; 800_000]).unwrap();
        fs::write(dir.join("F_small"), vec![0u8; 150_000]).unwrap();
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
                // A folder's chevron in its own level's colour; a tick box (not ticked) dim.
                let (at, fg) = match name {
                    "leaf" => (x + 2 * depth + 2, c.dim),
                    _ => (x + 2 * depth, c.depth[usize::from(depth) % 3]),
                };
                assert_eq!(buf[(at, y)].fg, fg, "{set:?} {name}");
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
    fn a_trash_scans_again_and_shows_the_tree_as_it_was() {
        let (_t, home) = home("view-refresh");
        let cache = home.join(".cache");
        for i in 0..30 {
            fs::write(cache.join(format!("pad{i:02}")), vec![0u8; 100]).unwrap();
        }
        let mut v = ready(policy(&home, None), &home);
        select(&mut v, ".cache");
        key(&mut v, KeyCode::Enter); // shows .cache
        let f3 = cache.join("big/inner/f3");
        let id = browse(&mut v).tree.find(&f3).unwrap();
        browse(&mut v).select(id); // opens big and inner
        text(&mut v, 120, 12);
        assert!(browse(&mut v).top > 0, "a short pane, scrolled");
        key(&mut v, KeyCode::Delete);
        age(&mut v);
        key(&mut v, KeyCode::Tab);
        browse(&mut v).confirm.as_mut().unwrap().focused -= REFOCUS;
        key(&mut v, KeyCode::Enter);
        assert!(!f3.exists());
        assert!(matches!(v.screen, Screen::Refreshing(..)), "scanning again");
        // Meanwhile the scan's card and the message; the last tree is gone, what it showed (as
        // the trash left it) kept by path.
        let lines = text(&mut v, 120, 12);
        assert!(has(&lines, "Moved to the trash: f3"), "{lines:#?}");
        assert!(has(&lines, "Scanning"), "{lines:#?}");
        let s = saved(&v);
        let (next, line) = (home.join(&s.selected), s.line);
        assert!(
            next.starts_with(cache.join("big/inner")) && next != f3,
            "{next:?}"
        );
        settle(&mut v);
        let b = browse(&mut v);
        assert!(b.tree.find(&f3).is_none(), "a new tree, from the disk");
        assert_eq!(b.tree.path(b.zoom), cache, "the same folder shown");
        let open: Vec<PathBuf> = b.open.iter().map(|&i| b.tree.path(i)).collect();
        for p in [cache.join("big"), cache.join("big/inner")] {
            assert!(open.contains(&p), "{} still open: {open:?}", p.display());
        }
        assert_eq!(
            b.tree.path(b.rows[b.cursor].0),
            next,
            "the same row selected"
        );
        text(&mut v, 120, 12);
        let b = browse(&mut v);
        assert_eq!(b.cursor - b.top, line, "on the same line");
    }

    #[test]
    fn r_scans_again_keeping_the_view_and_esc_stops_it_to_the_places() {
        let (_t, home) = home("view-rescan");
        let cache = home.join(".cache");
        let new = cache.join("big/inner/new");
        let inner_open = |v: &mut View| {
            let inner = browse(v).tree.find(&cache.join("big/inner")).unwrap();
            browse(v).select(inner);
            key(v, KeyCode::Right);
            assert_eq!(selected_name(v), "inner");
        };
        let mut v = ready(policy(&home, None), &cache);
        inner_open(&mut v);
        key(&mut v, KeyCode::Char('r'));
        assert!(matches!(v.screen, Screen::Refreshing(..)));
        assert_eq!(v.next_wake(), Some(SCAN_CHECK));
        assert!(has(&text(&mut v, 120, 40), "scanning again"));
        // Esc stops it. The last tree went when the scan started: the places, as after a first
        // scan stopped.
        key(&mut v, KeyCode::Esc);
        for _ in 0..5000 {
            v.tick();
            if !matches!(v.screen, Screen::Refreshing(..)) {
                break;
            }
            sleep(Duration::from_millis(1));
        }
        assert!(matches!(v.screen, Screen::Start(_)), "back to the places");
        // To the end: a file made meanwhile is there, inner still open and selected.
        let mut v = ready(policy(&home, None), &cache);
        inner_open(&mut v);
        fs::write(&new, b"x").unwrap();
        key(&mut v, KeyCode::Char('r'));
        settle(&mut v);
        assert!(browse(&mut v).tree.find(&new).is_some());
        assert!(
            row_shown(&mut v, "new") && row_shown(&mut v, "f0"),
            "inner open"
        );
        assert_eq!(selected_name(&mut v), "inner");
    }

    #[test]
    fn while_scanning_again_keys_and_clicks_wait_and_the_message_stays() {
        let (_t, home) = home("view-refresh-keys");
        let mut v = ready(policy(&home, None), &home.join(".cache"));
        text(&mut v, 120, 40);
        let cursor = browse(&mut v).cursor;
        key(&mut v, KeyCode::Char('r'));
        v.message = Some("kept".into());
        for code in [
            KeyCode::Down,
            KeyCode::Delete,
            KeyCode::Enter,
            KeyCode::Char('r'),
        ] {
            assert_eq!(key(&mut v, code), Outcome::None, "{code:?}");
        }
        let rows = v.hits.rows;
        assert_eq!(click(&mut v, rows.x + 2, rows.y + 1), Outcome::None);
        assert!(matches!(v.screen, Screen::Refreshing(..)));
        assert_eq!(v.message.as_deref(), Some("kept"));
        assert!(has(&text(&mut v, 120, 40), "kept"));
        assert_eq!(key(&mut v, KeyCode::Char('q')), Outcome::Quit);
        // None of them did anything: the same row selected, no dialog, once the tree is back.
        settle(&mut v);
        let b = browse(&mut v);
        assert!(b.cursor == cursor && b.confirm.is_none());
        assert_eq!(v.message.as_deref(), Some("kept"));
    }

    #[test]
    fn a_folder_gone_keeps_its_last_tree_and_says_so() {
        let (_t, home) = home("view-refresh-gone");
        let big = home.join(".cache/big");
        let mut v = ready(policy(&home, None), &big);
        fs::rename(&big, home.join(".cache/moved")).unwrap();
        key(&mut v, KeyCode::Char('r'));
        assert!(matches!(v.screen, Screen::Browse(_)), "the last tree stays");
        let msg = v.message.clone().unwrap();
        assert!(msg.starts_with("Cannot scan"), "{msg}");
        // After a trash, the message keeps what the trash did, and its check mark.
        v.message = Some("Moved to the trash: x".into());
        v.done = true;
        v.refresh();
        let msg = v.message.clone().unwrap();
        assert!(
            msg.starts_with("Moved to the trash: x; cannot scan "),
            "{msg}"
        );
        assert!(v.done);
    }

    #[test]
    fn a_scan_again_through_a_new_symlink_ends_and_keeps_the_view() {
        let (_t, home) = home("view-refresh-link");
        let cache = home.join(".cache");
        let mut v = ready(policy(&home, None), &cache.join("big"));
        select(&mut v, "inner");
        key(&mut v, KeyCode::Right);
        let f0 = browse(&mut v)
            .tree
            .find(&cache.join("big/inner/f0"))
            .unwrap();
        browse(&mut v).ticked.insert(f0);
        // The folder moves and a symlink takes its place: the scan again starts from the old
        // path, and its tree's root is the new one.
        fs::rename(cache.join("big"), cache.join("moved")).unwrap();
        std::os::unix::fs::symlink(cache.join("moved"), cache.join("big")).unwrap();
        key(&mut v, KeyCode::Char('r'));
        // On a thread of its own, so a tick that never ends fails the test instead of hanging.
        let ticks = std::thread::spawn(move || {
            settle(&mut v);
            v
        });
        let deadline = Instant::now() + Duration::from_secs(20);
        while !ticks.is_finished() {
            assert!(Instant::now() < deadline, "the scan again never ended");
            sleep(Duration::from_millis(5));
        }
        let mut v = ticks.join().unwrap();
        let moved = fs::canonicalize(cache.join("moved")).unwrap();
        assert_eq!(browse(&mut v).tree.root_path(), moved);
        assert_eq!(selected_name(&mut v), "inner");
        assert!(row_shown(&mut v, "f0"), "inner still open");
        // Ticks pick what Del moves: under a folder swapped for another, none stay.
        assert!(browse(&mut v).ticked.is_empty());
    }

    #[test]
    fn a_shown_folder_that_became_a_file_gives_way_to_the_folder_holding_it() {
        let (_t, home) = home("view-refresh-zoom-file");
        let cache = home.join(".cache");
        let mut v = ready(policy(&home, None), &cache);
        select(&mut v, "big");
        key(&mut v, KeyCode::Enter);
        select(&mut v, "inner");
        key(&mut v, KeyCode::Enter); // inner shown
        fs::rename(cache.join("big/inner"), home.join("inner-away")).unwrap();
        fs::write(cache.join("big/inner"), b"a file now").unwrap();
        key(&mut v, KeyCode::Char('r'));
        settle(&mut v);
        let b = browse(&mut v);
        assert_eq!(
            b.tree.path(b.zoom),
            cache.join("big"),
            "not the scanned folder"
        );
        assert_eq!(selected_name(&mut v), "inner");
    }

    #[test]
    fn a_folder_that_became_a_file_is_not_kept_open() {
        let (_t, home) = home("view-refresh-file");
        let cache = home.join(".cache");
        let mut v = ready(policy(&home, None), &cache);
        select(&mut v, "big");
        key(&mut v, KeyCode::Right);
        fs::rename(cache.join("big"), home.join("big-away")).unwrap();
        fs::write(cache.join("big"), b"a file now").unwrap();
        key(&mut v, KeyCode::Char('r'));
        settle(&mut v);
        let b = browse(&mut v);
        assert!(
            b.open.iter().all(|&i| b.tree.node(i).is_dir()),
            "{:?}",
            b.open
        );
        assert_eq!(selected_name(&mut v), "big");
    }

    #[test]
    fn a_click_before_the_new_tree_is_drawn_finds_no_tile_of_the_last_one() {
        let (_t, home) = home("view-refresh-hits");
        let cache = home.join(".cache");
        let mut v = ready(policy(&home, None), &cache);
        select(&mut v, "big");
        key(&mut v, KeyCode::Enter);
        select(&mut v, "inner");
        key(&mut v, KeyCode::Enter); // the map: inner's twenty files
        text(&mut v, 120, 40);
        let tile = v.hits.tiles.iter().find(|t| t.1.is_some()).unwrap().0;
        // The new tree has none of them, nor that many entries.
        fs::rename(cache.join("big"), home.join("big-away")).unwrap();
        key(&mut v, KeyCode::Char('r'));
        settle(&mut v); // and no drawing after it
        assert!(v.hits.tiles.is_empty() && v.hits.locks.is_empty());
        let b = browse(&mut v);
        let before = (b.cursor, b.zoom);
        v.last_click = None;
        assert_eq!(click(&mut v, tile.x + 1, tile.y), Outcome::None);
        let b = browse(&mut v);
        assert_eq!((b.cursor, b.zoom), before);
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

    #[test]
    fn every_tile_writes_its_text_in_bold() {
        use ratatui::style::Modifier;
        let (_t, home) = home("view-tile-bold");
        let mixed = home.join(".cache/mixed");
        fs::create_dir(&mixed).unwrap();
        fs::write(mixed.join("a"), vec![0u8; 120_000]).unwrap();
        fs::write(mixed.join("b"), vec![0u8; 100_000]).unwrap();
        for i in 0..20 {
            fs::write(mixed.join(format!("s{i}")), vec![0u8; 3000]).unwrap();
        }
        let (mut others, mut rest) = (0, 0);
        // inner: twenty tiles of their own; mixed at 40x12: a, b and a "+20 items" tile.
        for (dir, w, h) in [(home.join(".cache/big/inner"), 120, 40), (mixed, 40, 12)] {
            let mut v = ready(policy(&home, None), &dir);
            key(&mut v, KeyCode::Char('a')); // apparent sizes: the lengths, whatever the disk does
            v.panes = Panes::Map;
            let buf = draw_buf(&mut v, w, h);
            let b = browse(&mut v);
            let selected = b.rows[b.cursor].0;
            for (r, id) in v.hits.tiles.clone() {
                if r.width < 3 {
                    continue;
                }
                let bold = |x: u16, y: u16| buf[(x, y)].modifier.contains(Modifier::BOLD);
                assert!(
                    bold(r.x + 1, r.y),
                    "{id:?}: the class letter and name, or +N items"
                );
                // The "+N items" tile has no size line.
                if r.height >= 2 && id.is_some() {
                    assert!(bold(r.x + 1, r.y + 1), "{id:?}: the size");
                }
                others += usize::from(id != Some(selected));
                rest += usize::from(id.is_none());
            }
        }
        assert!(others > 0, "tiles besides the selected one");
        assert_eq!(rest, 1, "the small items' tile");
    }

    #[test]
    fn the_columns_stay_in_the_pane_at_every_width_with_a_long_wide_name() {
        let (_t, home) = home("view-wide-name");
        let cache = home.join(".cache");
        let name = "很長的中文檔案名稱用來看欄寬在每個寬度是否都正確.txt";
        fs::write(cache.join(name), vec![0u8; 5000]).unwrap();
        let mut v = ready(policy(&home, None), &cache);
        for panes in [Panes::Both, Panes::Tree] {
            v.panes = panes;
            for w in 24..=140 {
                let buf = draw_buf(&mut v, w, 12);
                let (heads, pane) = (v.hits.heads.clone(), v.hits.rows);
                for (i, (at, field)) in heads.iter().enumerate() {
                    let here = format!("{panes:?} at {w}: {at:?} in {pane:?}");
                    assert!(at.x >= pane.x && at.right() <= pane.right(), "{here}");
                    // The heading drawn in its own cells: its title, then the sort arrow.
                    let drawn: String = (at.x..at.right())
                        .map(|x| buf[(x, at.y)].symbol())
                        .collect();
                    let title = drawn.trim().trim_end_matches(['▴', '▾']);
                    let want: &[&str] = match field {
                        Field::Name => &["Name"],
                        Field::Size => &["Usage", "Size"],
                        Field::Items => &["Items"],
                        Field::Modified => &["Modified"],
                    };
                    assert!(want.contains(&title), "{here}: {drawn:?}");
                    // The rows' column lines are under the headings' ones.
                    if i > 0 {
                        assert_eq!(buf[(at.x - 1, pane.y)].symbol(), "│", "{here}");
                    }
                }
            }
        }
    }

    #[test]
    fn the_title_ends_with_the_version_and_the_path_gives_way_first() {
        let (_t, home) = home("view-version");
        let version = format!("termdisk v{}", env!("CARGO_PKG_VERSION"));
        let cache = home.join(".cache");
        let mut views = [
            View::new(policy(&home, None), None),
            View::new(policy(&home, None), Some(&cache)),
            ready(policy(&home, None), &cache),
        ];
        for v in &mut views {
            for w in 24..=140 {
                let top: Vec<char> = text(v, w, 12)[0].chars().collect();
                let line: String = top.iter().collect();
                // Whole wherever the title's room (8 cells less than the bar) holds it; else
                // cut at its end only.
                let room = w as usize - 8;
                let shown = match version.len() <= room {
                    true => version.clone(),
                    false => format!("{}…", &version[..room - 1]),
                };
                assert!(line.contains(&shown), "{w}: {line:?}");
                assert_eq!(top[w as usize - 4], ' ', "{w}: clear of the close button");
            }
        }
        // The tree's title: the whole path where there is room; narrower, the path is cut in
        // its middle while the size stays; the version stays at every width.
        let tree = &mut views[2];
        let path = cache.display().to_string();
        let top = text(tree, 140, 12).swap_remove(0);
        assert!(
            top.contains(&path) && top.contains("(disk usage)"),
            "{top:?}"
        );
        let cut_path_kept_size = (24..140).any(|w| {
            let top = text(tree, w, 12).swap_remove(0);
            top.contains('…') && top.contains("(disk usage)")
        });
        assert!(cut_path_kept_size, "the path gives way before the size");
    }

    #[test]
    fn names_take_the_cells_the_columns_do_not_need() {
        let (_t, home) = home("view-name-room");
        let cache = home.join(".cache");
        fs::write(cache.join("IMG_4102_holiday.jpeg"), vec![0u8; 5000]).unwrap();
        let mut v = ready(policy(&home, None), &cache);
        let lines = text(&mut v, 120, 40);
        assert!(has(&lines, "IMG_4102_holiday.jpeg"), "{lines:#?}");
        // Usage's bar gave cells to the name, and still shows.
        assert!(has(&lines, "100% ━") && !has(&lines, "100% ━━━━━━━━"));
        // Where the names fit, the bar keeps its 8 cells.
        let mut v = ready(policy(&home, None), &cache.join("big/inner"));
        assert!(has(&text(&mut v, 120, 40), "100% ━━━━━━━━"));
    }

    #[test]
    fn a_folder_that_only_holds_a_locked_item_has_no_lock_and_del_says_why() {
        let (_t, home) = home("view-holds");
        fs::create_dir_all(home.join(".cargo/registry")).unwrap();
        fs::write(home.join(".cargo/registry/crate"), vec![0u8; 5000]).unwrap();
        fs::write(home.join(".cargo/credentials.toml"), b"token").unwrap();
        let mut v = ready(policy(&home, None), &home);
        let lines = text(&mut v, 120, 40);
        assert!(
            has(&lines, ".cargo/") && !has(&lines, ".cargo/ ⊘"),
            "{lines:#?}"
        );
        assert!(has(&lines, ".config/ ⊘"), "protected itself: still locked");
        select(&mut v, ".cargo");
        key(&mut v, KeyCode::Delete);
        assert!(
            browse(&mut v).confirm.is_none(),
            "the guard still refuses it"
        );
        let msg = v.message.clone().unwrap();
        let want = "Cannot move to the trash: it holds 1 locked item; protected: ";
        assert!(msg.starts_with(want), "{msg}");
        assert!(msg.ends_with("/.cargo/credentials.toml"), "{msg}");
        assert!(home.join(".cargo/credentials.toml").exists());
        // Inside it, the locked item itself has the lock.
        key(&mut v, KeyCode::Right);
        assert!(has(&text(&mut v, 120, 40), "credentials.toml ⊘"));
    }

    /// The line and column where `s` starts in `lines`, in cells.
    fn find_text(lines: &[String], s: &str) -> Option<(usize, usize)> {
        lines.iter().enumerate().find_map(|(y, l)| {
            let i = l.find(s)?;
            Some((y, l[..i].chars().count()))
        })
    }

    #[test]
    fn a_lock_under_the_pointer_says_why_and_a_click_there_selects_its_row() {
        let (_t, home) = home("view-tip");
        fs::write(home.join(".config/app/big"), vec![0u8; 600_000]).unwrap();
        let mut v = ready(policy(&home, None), &home);
        select(&mut v, "Downloads");
        text(&mut v, 120, 40);
        let config = browse(&mut v).tree.find(&home.join(".config")).unwrap();
        let rows = v.hits.rows;
        let mine = |v: &View, in_tree: bool| {
            let at = v
                .hits
                .locks
                .iter()
                .find(|l| l.1 == config && (l.0.x < rows.right()) == in_tree);
            at.expect("a lock for .config").0
        };
        let lock = mine(&v, true);
        let why = "protected: ~/.config, with everything in it";
        // Onto the lock: drawn again, the reason in a framed box.
        assert_eq!(hover(&mut v, lock.x + 1, lock.y), Outcome::Redraw);
        assert_eq!(v.hover, Hover::Lock(config));
        let lines = text(&mut v, 120, 40);
        let (y, x) = find_text(&lines, why).expect("the reason");
        assert_eq!(
            lines[y].chars().nth(x - 2),
            Some('│'),
            "a frame: {:?}",
            lines[y]
        );
        assert_eq!(
            hover(&mut v, lock.x, lock.y),
            Outcome::None,
            "the space before it too"
        );
        let again = find_text(&text(&mut v, 120, 40), why);
        assert_eq!(
            again,
            Some((y, x)),
            "the box stays at the lock, wherever on it the pointer is"
        );
        // Along the row, off the lock: gone.
        hover(&mut v, rows.x + 2, lock.y);
        assert!(!has(&text(&mut v, 120, 40), why));
        // A click on the lock is a click on its row.
        hover(&mut v, lock.x + 1, lock.y);
        click(&mut v, lock.x + 1, lock.y);
        assert_eq!(selected_name(&mut v), ".config");
        // The lock on its tile says the same.
        text(&mut v, 120, 40);
        let tile = mine(&v, false);
        hover(&mut v, tile.x + 1, tile.y);
        assert_eq!(v.hover, Hover::Lock(config));
        assert!(has(&text(&mut v, 120, 40), why));
    }

    #[test]
    fn the_reason_shows_in_a_frame_in_ascii_and_16_colours() {
        use crate::style::{Colors, Depth};
        let (_t, home) = home("view-tip-ascii");
        let mut v = ready(policy(&home, None), &home);
        v.set_icons(IconSet::Ascii);
        v.set_colors(Colors::default().fitted(Depth::C16));
        text(&mut v, 120, 40);
        let config = browse(&mut v).tree.find(&home.join(".config")).unwrap();
        let lock = v.hits.locks.iter().find(|l| l.1 == config).unwrap().0;
        hover(&mut v, lock.x + 1, lock.y);
        let lines = text(&mut v, 120, 40);
        let (y, x) = find_text(&lines, "protected: ~/.config, with").expect("the reason");
        let cell = |dy: usize, dx: usize| lines[y + dy - 1].chars().nth(x + dx - 2);
        assert_eq!(
            (cell(0, 0), cell(1, 0), cell(2, 0)),
            (Some('+'), Some('|'), Some('+'))
        );
        // In 16 colours the box is as black as the pane: the frame is what shows it.
        assert_eq!(v.colors.widget_bg, v.colors.side_bg);
    }

    #[test]
    fn the_reason_names_every_lock_of_an_item() {
        use crate::tree::{DIR, FILE, MOUNT, NOT_OWNED, UNREADABLE};
        let (_t, home) = home("view-reasons");
        let p = policy(&home, None);
        let mut t = Tree::new(PathBuf::from("/nowhere/r"), 1);
        t.push(0, b"", 0, 0, 1, 0, DIR);
        t.add_children(0, 1, 3);
        t.push(0, b"mnt", 0, 0, 2, 0, DIR | MOUNT);
        t.push(0, b"odd", 0, 0, 3, 0, DIR | UNREADABLE | NOT_OWNED);
        t.push(0, b"f", 0, 0, 4, 0, FILE);
        t.finish();
        assert_eq!(lock_reason(&t, 1, &p), "a mount point: /nowhere/r/mnt");
        // From the scan's flags and the policy's paths alone: the view never waits on the disk
        // to draw (/nowhere does not exist).
        let odd = "not yours: another user owns it; could not be read";
        assert_eq!(lock_reason(&t, 2, &p), odd);
        // Protected: itself only, with everything in it, or inside such a path.
        let mut v = ready(policy(&home, None), &home);
        let Screen::Browse(b) = &mut v.screen else {
            panic!("not browsing")
        };
        let why =
            |rel: &str| lock_reason(&b.tree, b.tree.find(&home.join(rel)).unwrap(), &v.policy);
        assert_eq!(
            why("Downloads"),
            "protected: ~/Downloads (what is in it may go)"
        );
        assert_eq!(
            why(".config"),
            "protected: ~/.config, with everything in it"
        );
        assert_eq!(why(".config/app/rc"), "protected: inside ~/.config");
        // Not the user's, as the scan saw it.
        let (data, config) = (home.join(".local/share"), home.join(".config"));
        let homes = [home.clone()];
        let other = Policy::new(me() + 1, None, &homes, &data, &config, None, None);
        let mut v = ready(other, &home.join(".cache"));
        let Screen::Browse(b) = &mut v.screen else {
            panic!("not browsing")
        };
        let small = b.tree.find(&home.join(".cache/small")).unwrap();
        let reason = lock_reason(&b.tree, small, &v.policy);
        assert_eq!(reason, "not yours: another user owns it");
    }

    #[test]
    fn space_ticks_and_moves_down_and_the_shown_folder_takes_no_tick() {
        use crate::style::{Colors, Depth};
        let (_t, home) = home("view-tick");
        let mut v = ready(policy(&home, None), &home.join(".cache"));
        key(&mut v, KeyCode::Home);
        key(&mut v, KeyCode::Char(' '));
        assert!(ticked(&mut v).is_empty(), "the shown folder takes no tick");
        assert_eq!(selected_name(&mut v), "big", "the cursor moved down");
        key(&mut v, KeyCode::Char(' '));
        key(&mut v, KeyCode::Char(' '));
        assert_eq!(ticked(&mut v), ["big", "linked"]);
        assert_eq!(selected_name(&mut v), "small");
        let buf = draw_buf(&mut v, 120, 40);
        let lines = text(&mut v, 120, 40);
        assert!(
            has(&lines, "▸ ☑ big/") && has(&lines, "  ☑ linked") && has(&lines, "  ☐ small"),
            "{lines:#?}"
        );
        let y = row_y(&mut v, "linked");
        assert_eq!(
            buf[(v.hits.rows.x + 3, y)].bg,
            v.colors.tick_bg,
            "a tint too"
        );
        let status = &lines[39];
        assert!(status.starts_with(" 2 ticked, "), "{status}");
        assert!(
            status.contains("Del trash them") && status.contains("Esc clear"),
            "{status}"
        );
        // Space again takes the tick off.
        select(&mut v, "linked");
        key(&mut v, KeyCode::Char(' '));
        assert_eq!(ticked(&mut v), ["big"]);
        // Without colours: the ASCII boxes.
        v.set_icons(IconSet::Ascii);
        v.set_colors(Colors::default().fitted(Depth::C16));
        let lines = text(&mut v, 120, 40);
        assert!(
            has(&lines, "> [x] big/") && has(&lines, "  [ ] linked"),
            "{lines:#?}"
        );
        // The keys list them.
        key(&mut v, KeyCode::Char('?'));
        assert!(has(&text(&mut v, 120, 40), "Space, box, drag"));
    }

    /// The tick box of the row named `name`, as last drawn, and its item.
    fn box_of(v: &mut View, name: &str) -> (Rect, NodeId) {
        let b = browse(v);
        let id = b.rows.iter().map(|r| r.0).find(|&i| b.tree.name(i) == name);
        let id = id.unwrap();
        (v.hits.boxes.iter().find(|h| h.1 == id).unwrap().0, id)
    }

    #[test]
    fn a_short_list_never_scrolls_its_top_rows_away() {
        // A scan again with a new row above the selected one: the selected item keeps its line
        // only while the list is longer than the pane.
        let (_t, home) = home("view-short");
        let cache = home.join(".cache");
        let mut v = ready(policy(&home, None), &cache);
        select(&mut v, "big");
        text(&mut v, 120, 40);
        assert_eq!(browse(&mut v).top, 0);
        fs::create_dir(cache.join("huge")).unwrap(); // a folder: folders come first
        fs::write(cache.join("huge/f"), vec![9u8; 900_000]).unwrap();
        key(&mut v, KeyCode::Char('r'));
        settle(&mut v);
        let lines = text(&mut v, 120, 40);
        assert_eq!(selected_name(&mut v), "big");
        assert_eq!(browse(&mut v).cursor, 2, "huge came above it");
        assert_eq!(
            browse(&mut v).top,
            0,
            "nothing hidden above, no empty lines below"
        );
        let first = &lines[usize::from(v.hits.rows.y)];
        assert!(
            first.starts_with(" ▾ /"),
            "the scanned folder's row: {lines:#?}"
        );
    }

    #[test]
    fn a_click_on_a_tick_box_ticks_its_row_and_moves_the_cursor_there() {
        use ratatui::style::Modifier;
        let (_t, home) = home("view-box");
        // Read-only too: only Del is off there.
        let mut v = ready(
            policy(&home, Some("read-only: test".into())),
            &home.join(".cache"),
        );
        key(&mut v, KeyCode::Home);
        let buf = draw_buf(&mut v, 120, 40);
        // Every row has its box but the shown folder's.
        let (rows, zoom) = (browse(&mut v).rows.len(), browse(&mut v).zoom);
        assert_eq!(v.hits.boxes.len(), rows - 1);
        assert!(!v.hits.boxes.iter().any(|h| h.1 == zoom));
        let top: String = (0..40).map(|x| buf[(x, v.hits.rows.y)].symbol()).collect();
        assert!(!top.contains('☐') && !top.contains('☑'), "{top}");
        let (small, id) = box_of(&mut v, "small");
        assert_eq!(buf[(small.x, small.y)].symbol(), "☐");
        assert_eq!(buf[(small.x, small.y)].fg, v.colors.dim);
        // The pointer on it underlines it.
        mouse(&mut v, MouseEventKind::Moved, small.x, small.y);
        assert_eq!(v.hover, Hover::Box(id));
        let buf = draw_buf(&mut v, 120, 40);
        assert!(buf[(small.x, small.y)]
            .modifier
            .contains(Modifier::UNDERLINED));
        // A click ticks it and puts the cursor there.
        click(&mut v, small.x, small.y);
        assert_eq!(ticked(&mut v), ["small"]);
        assert_eq!(selected_name(&mut v), "small");
        select(&mut v, "linked"); // the row's own colours, not the selection's
        let buf = draw_buf(&mut v, 120, 40);
        assert_eq!(buf[(small.x, small.y)].symbol(), "☑");
        assert_eq!(buf[(small.x, small.y)].fg, v.colors.tick_fg);
        // Two quick clicks on a folder's box: ticked, unticked, and no double click (the
        // folder neither opens nor closes).
        let (big, _) = box_of(&mut v, "big");
        let open = browse(&mut v).open.len();
        click(&mut v, big.x, big.y);
        assert_eq!(ticked(&mut v), ["big", "small"]);
        click(&mut v, big.x, big.y);
        assert_eq!(ticked(&mut v), ["small"]);
        assert_eq!(browse(&mut v).open.len(), open);
        assert_eq!(selected_name(&mut v), "big");
    }

    /// `.cache` with big and inner open (and a file `side` in big, beside inner), big ticked.
    fn big_ticked(tag: &str) -> (TempDir, PathBuf, View) {
        let (t, home) = home(tag);
        let cache = home.join(".cache");
        fs::write(cache.join("big/side"), vec![4u8; 5000]).unwrap();
        let mut v = ready(policy(&home, None), &cache);
        let f0 = browse(&mut v)
            .tree
            .find(&cache.join("big/inner/f0"))
            .unwrap();
        browse(&mut v).select(f0); // opens big and inner
        select(&mut v, "big");
        key(&mut v, KeyCode::Char(' '));
        assert_eq!(ticked(&mut v), ["big"]);
        (t, cache, v)
    }

    #[test]
    fn a_ticked_folder_ticks_what_is_in_it_and_unticking_one_item_splits_it() {
        let (_t, cache, mut v) = big_ticked("view-split");
        let lines = text(&mut v, 120, 40);
        for s in ["☑ big/", "☑ inner/", "☑ f3", "☑ side"] {
            assert!(has(&lines, s), "{s}: {lines:#?}");
        }
        let f3 = browse(&mut v)
            .tree
            .find(&cache.join("big/inner/f3"))
            .unwrap();
        let b = browse(&mut v);
        let (all, one) = (b.ticked_size(false), counted(b.tree.node(f3), false));
        // Space on f3: big's tick goes, what was in it but f3 stays ticked, each on its own.
        browse(&mut v).select(f3);
        key(&mut v, KeyCode::Char(' '));
        let names = ticked(&mut v);
        let files = (0..20).filter(|&i| i != 3).map(|i| format!("f{i}"));
        let mut want: Vec<String> = files.chain(["side".into()]).collect();
        want.sort();
        assert_eq!(names, want);
        assert_eq!(browse(&mut v).ticked_size(false), all - one);
        let lines = text(&mut v, 120, 40);
        for s in ["◩ big/", "◩ inner/", "☐ f3", "☑ f4", "☑ side"] {
            assert!(has(&lines, s), "{s}: {lines:#?}");
        }
        // The count is of what is ticked on its own, as Del lists it.
        assert!(lines[39].starts_with(" 20 ticked, "), "{}", lines[39]);
        // Del moves those, and f3 stays.
        key(&mut v, KeyCode::Delete);
        let c = browse(&mut v).confirm.as_ref().unwrap();
        let mut paths: Vec<PathBuf> = c.items.iter().map(|i| i.1.clone()).collect();
        paths.sort();
        let mut expect: Vec<PathBuf> = want
            .iter()
            .map(|n| match n.as_str() {
                "side" => cache.join("big/side"),
                f => cache.join("big/inner").join(f),
            })
            .collect();
        expect.sort();
        assert_eq!(paths, expect);
        text(&mut v, 120, 40); // every path shown
        arm(&mut v);
        key(&mut v, KeyCode::Enter);
        settle(&mut v);
        assert!(cache.join("big/inner/f3").exists());
        assert!(!cache.join("big/inner/f4").exists() && !cache.join("big/side").exists());
    }

    #[test]
    fn a_folder_holding_ticks_shows_it_and_a_click_on_that_box_ticks_the_folder() {
        let (_t, home) = home("view-part");
        let inner = home.join(".cache/big/inner");
        let mut v = ready(policy(&home, None), &home.join(".cache"));
        tick(&mut v, &inner.join("f0"));
        let lines = text(&mut v, 120, 40);
        for s in ["◩ big/", "◩ inner/", "☑ f0", "☐ f1"] {
            assert!(has(&lines, s), "{s}: {lines:#?}");
        }
        // Every file in it ticked: still not the folder itself (Del would move more).
        for i in 1..20 {
            tick(&mut v, &inner.join(format!("f{i}")));
        }
        assert_eq!(ticked(&mut v).len(), 20);
        assert!(has(&text(&mut v, 120, 40), "◩ inner/"));
        // Ticked, it shows ticked, never partly; the ticks in it go with it.
        tick(&mut v, &inner);
        assert_eq!(ticked(&mut v), ["inner"]);
        let lines = text(&mut v, 120, 40);
        assert!(
            has(&lines, "☑ inner/") && has(&lines, "◩ big/"),
            "{lines:#?}"
        );
        // A click on big's box ticks big, and big alone.
        let (big, _) = box_of(&mut v, "big");
        click(&mut v, big.x, big.y);
        assert_eq!(ticked(&mut v), ["big"]);
        let lines = text(&mut v, 120, 40);
        assert!(has(&lines, "☑ big/") && has(&lines, "☑ inner/") && !has(&lines, "◩"));
        // ASCII: the same width.
        v.set_icons(IconSet::Ascii);
        tick(&mut v, &inner.join("f0")); // splits big
        assert!(has(&text(&mut v, 120, 40), "[-] big/"));
    }

    #[test]
    fn a_ticked_home_or_wholly_protected_folder_is_not_opened_up() {
        let (_t, home) = home("view-tick-home");
        let one = "Cannot move to the trash: the ticked item cannot go; protected: ";
        // ~/.local: protected with all in it (the data home is in it).
        let mut v = ready(policy(&home, None), &home);
        tick(&mut v, &home.join(".local"));
        key(&mut v, KeyCode::Delete);
        assert!(browse(&mut v).confirm.is_none());
        let msg = v.message.clone().unwrap();
        assert!(msg.starts_with(one), "{msg}");
        // The home itself, from the folder above it: never everything in it that may go.
        let mut v = ready(policy(&home, None), home.parent().unwrap());
        tick(&mut v, &home);
        key(&mut v, KeyCode::Delete);
        assert!(browse(&mut v).confirm.is_none());
        let msg = v.message.clone().unwrap();
        assert!(msg.starts_with(one), "{msg}");
    }

    #[test]
    fn a_ticked_folder_holding_a_locked_item_moves_all_but_it() {
        let (_t, home) = home("view-tick-lock");
        let (big, inner) = (home.join(".cache/big"), home.join(".cache/big/inner"));
        fs::create_dir_all(home.join(".config/termdisk")).unwrap();
        let protect = "~/.cache/big/inner/f7\n";
        fs::write(home.join(".config/termdisk/protect"), protect).unwrap();
        let mut v = ready(policy(&home, None), &home.join(".cache"));
        tick(&mut v, &big);
        key(&mut v, KeyCode::Delete);
        let c = browse(&mut v).confirm.as_ref().unwrap();
        // The locked item and the folders on its way are not moved; the rest is.
        let refused: Vec<&PathBuf> = c.refused.iter().map(|r| &r.0).collect();
        assert_eq!(refused, [&big, &inner, &inner.join("f7")]);
        let files = (0..20)
            .filter(|&i| i != 7)
            .map(|i| inner.join(format!("f{i}")));
        let mut want: Vec<PathBuf> = files.collect();
        want.sort();
        let items: Vec<PathBuf> = c.items.iter().map(|i| i.1.clone()).collect();
        assert_eq!(items, want);
    }

    #[test]
    fn a_drag_off_over_a_folder_holding_ticks_takes_those_off_too() {
        let (_t, home) = home("view-drag-part");
        let cache = home.join(".cache");
        let mut v = ready(policy(&home, None), &cache);
        tick(&mut v, &cache.join("big/inner/f0")); // big and inner ◩
        tick(&mut v, &cache.join("small"));
        text(&mut v, 120, 40);
        let (small, _) = box_of(&mut v, "small");
        let (big, _) = box_of(&mut v, "big");
        mouse(
            &mut v,
            MouseEventKind::Down(MouseButton::Left),
            small.x,
            small.y,
        );
        assert_eq!(ticked(&mut v), ["f0"]);
        mouse(
            &mut v,
            MouseEventKind::Drag(MouseButton::Left),
            small.x,
            big.y,
        );
        assert!(ticked(&mut v).is_empty(), "big painted ☐, f0 with it");
        assert!(!has(&text(&mut v, 120, 40), "◩"));
    }

    #[test]
    fn a_drag_off_over_rows_in_a_ticked_folder_splits_its_tick() {
        let (_t, cache, mut v) = big_ticked("view-drag-split");
        text(&mut v, 120, 40);
        // Rows: .cache, big, inner, inner's 20 files, side, linked, small.
        let b = browse(&mut v);
        let name = |i: usize| b.tree.name(b.rows[i].0).to_string_lossy().into_owned();
        let (a, z) = (name(4), name(6));
        let off: Vec<String> = (4..=6).map(name).collect();
        let (first, _) = box_of(&mut v, &a);
        let (last, _) = box_of(&mut v, &z);
        mouse(
            &mut v,
            MouseEventKind::Down(MouseButton::Left),
            first.x,
            first.y,
        );
        let drag = MouseEventKind::Drag(MouseButton::Left);
        mouse(&mut v, drag, first.x, last.y);
        let names = ticked(&mut v);
        assert_eq!(names.len(), 19 - 2 + 1, "{names:?}"); // inner's other 17, and side
        assert!(off.iter().all(|n| !names.contains(n)), "{off:?} {names:?}");
        assert!(names.contains(&"side".to_string()) && !names.contains(&"big".to_string()));
        mouse(
            &mut v,
            MouseEventKind::Up(MouseButton::Left),
            first.x,
            last.y,
        );
        select(&mut v, "big");
        key(&mut v, KeyCode::Char(' ')); // big ticked again, the ticks in it gone
        assert_eq!(ticked(&mut v), ["big"]);
        // Painting on over rows in a ticked folder changes nothing.
        text(&mut v, 120, 40);
        let rows = v.hits.rows;
        let top = browse(&mut v).top;
        let (y4, y6) = (rows.y + (4 - top) as u16, rows.y + (6 - top) as u16);
        mouse(
            &mut v,
            MouseEventKind::Down(MouseButton::Left),
            rows.x + 3,
            y4,
        );
        mouse(&mut v, drag, rows.x + 3, y6);
        assert_eq!(ticked(&mut v), ["big"]);
        assert!(cache.join("big/inner/f4").exists());
    }

    #[test]
    fn a_drag_from_a_tick_box_gives_the_rows_the_state_it_took() {
        let (_t, home) = home("view-box-drag");
        let mut v = ready(policy(&home, None), &home.join(".cache"));
        for _ in 0..3 {
            key(&mut v, KeyCode::Char(' ')); // big, linked, small
        }
        text(&mut v, 120, 40);
        let ((small, _), (big, _)) = (box_of(&mut v, "small"), box_of(&mut v, "big"));
        let drag = MouseEventKind::Drag(MouseButton::Left);
        mouse(
            &mut v,
            MouseEventKind::Down(MouseButton::Left),
            small.x,
            small.y,
        );
        assert_eq!(ticked(&mut v), ["big", "linked"], "the click unticked it");
        mouse(&mut v, drag, small.x, big.y);
        assert!(
            ticked(&mut v).is_empty(),
            "and the drag unticks the rows it goes over"
        );
        mouse(
            &mut v,
            MouseEventKind::Up(MouseButton::Left),
            small.x,
            big.y,
        );
        mouse(
            &mut v,
            MouseEventKind::Down(MouseButton::Left),
            big.x,
            big.y,
        );
        mouse(&mut v, drag, big.x, small.y);
        assert_eq!(ticked(&mut v), ["big", "linked", "small"]);
    }

    /// A trash in `home` holding two items, as a file manager leaves it.
    fn fill_trash(home: &Path) -> PathBuf {
        let t = home.join(".local/share/Trash");
        fs::create_dir_all(t.join("files/old/deep")).unwrap();
        fs::create_dir_all(t.join("info")).unwrap();
        fs::write(t.join("files/old/deep/x"), vec![5u8; 50_000]).unwrap();
        fs::write(t.join("files/note.txt"), b"note").unwrap();
        for n in ["old", "note.txt"] {
            let info = "[Trash Info]\nPath=/x\nDeletionDate=2026-01-01T00:00:00\n";
            fs::write(t.join(format!("info/{n}.trashinfo")), info).unwrap();
        }
        t
    }

    /// The start screen, the Trash place selected.
    fn on_trash_place(policy: Policy) -> View {
        let mut v = View::new(policy, None);
        let Screen::Start(s) = &mut v.screen else {
            panic!("not the start screen")
        };
        s.sel = s.places.iter().position(|p| p.name == "Trash").unwrap();
        v
    }

    #[test]
    fn del_on_the_trash_place_asks_then_empties_it_for_good() {
        let (_t, home) = home("view-empty");
        let trash = fill_trash(&home);
        let mut v = on_trash_place(policy(&home, None));
        assert!(has(&text(&mut v, 120, 40), "Del empty the trash"));
        key(&mut v, KeyCode::Delete);
        settle(&mut v); // the trash's scan, then its tree with the dialog over it
        let c = browse(&mut v).confirm.as_ref().unwrap();
        assert!(c.empty && !c.on_move);
        let lines = text(&mut v, 120, 40);
        for s in [
            "Empty the trash?",
            "2 items, ",
            "deleted permanently",
            "cannot be undone",
            "[ Empty Trash ]",
            &trash.display().to_string(),
        ] {
            assert!(has(&lines, s), "{s}: {lines:#?}");
        }
        // The trash dialog's gate: Tab at once, Enter only after the wait.
        key(&mut v, KeyCode::Tab);
        key(&mut v, KeyCode::Enter);
        assert!(browse(&mut v).confirm.is_some());
        assert!(trash.join("files/note.txt").exists());
        age(&mut v);
        key(&mut v, KeyCode::Enter);
        assert!(browse(&mut v).emptying.is_some());
        assert!(v.next_wake().is_some(), "watches it");
        assert!(has(&text(&mut v, 120, 40), "Emptying the trash"));
        assert_eq!(key(&mut v, KeyCode::Down), Outcome::None, "keys wait");
        for _ in 0..5000 {
            v.tick();
            if !matches!(&v.screen, Screen::Browse(b) if b.emptying.is_some()) {
                break;
            }
            sleep(Duration::from_millis(1));
        }
        settle(&mut v); // then the trash scanned again
        let lines = text(&mut v, 120, 40);
        assert!(
            has(&lines, "Emptied the trash: 2 items deleted"),
            "{lines:#?}"
        );
        for d in ["files", "info"] {
            assert_eq!(fs::read_dir(trash.join(d)).unwrap().count(), 0, "{d}");
        }
        // Del on its row again: nothing in it.
        key(&mut v, KeyCode::Home);
        key(&mut v, KeyCode::Delete);
        assert!(browse(&mut v).confirm.is_none());
        assert_eq!(v.message.as_deref(), Some("The trash is empty"));
    }

    #[test]
    fn emptying_the_trash_is_off_when_read_only_and_asked_from_its_row() {
        let (_t, home) = home("view-empty-ro");
        let trash = fill_trash(&home);
        let ro = policy(&home, Some("read-only: test".into()));
        let mut v = on_trash_place(ro);
        assert!(!has(&text(&mut v, 120, 40), "Del empty"));
        assert_eq!(key(&mut v, KeyCode::Delete), Outcome::None);
        assert!(matches!(v.screen, Screen::Start(_)));
        // Browsing the trash: Del on its own row asks to empty it, and Esc keeps it all.
        let mut v = ready(policy(&home, None), &trash);
        key(&mut v, KeyCode::Home);
        assert!(has(&text(&mut v, 120, 40), "Del empty the trash"));
        key(&mut v, KeyCode::Delete);
        assert!(browse(&mut v).confirm.as_ref().is_some_and(|c| c.empty));
        key(&mut v, KeyCode::Esc);
        assert!(browse(&mut v).confirm.is_none());
        assert!(trash.join("files/old/deep/x").exists());
    }

    #[test]
    fn dragging_over_rows_ticks_them_and_dragging_back_unticks() {
        let (_t, home) = home("view-drag");
        let cache = home.join(".cache");
        let mut v = ready(policy(&home, None), &cache);
        let f0 = browse(&mut v)
            .tree
            .find(&cache.join("big/inner/f0"))
            .unwrap();
        browse(&mut v).select(f0); // big and inner open
        text(&mut v, 120, 40);
        let (rows, x) = (v.hits.rows, v.hits.rows.x + 3);
        let at = |v: &mut View, i: usize| rows.y + (i - browse(v).top) as u16;
        let names = |v: &mut View, r: std::ops::RangeInclusive<usize>| {
            let b = browse(v);
            let mut n: Vec<String> = b.rows[r]
                .iter()
                .map(|r| b.tree.name(r.0).to_string_lossy().into_owned())
                .collect();
            n.sort();
            n
        };
        // From the first of inner's files (rows: .cache, big, inner, twenty files, two more).
        let a = 3;
        let (ya, y2) = (at(&mut v, a), at(&mut v, a + 2));
        mouse(&mut v, MouseEventKind::Down(MouseButton::Left), x, ya);
        assert!(ticked(&mut v).is_empty(), "a click ticks nothing");
        let drag = MouseEventKind::Drag(MouseButton::Left);
        assert_eq!(mouse(&mut v, drag, x, y2), Outcome::Redraw);
        assert_eq!(ticked(&mut v), names(&mut v, a..=a + 2));
        assert_eq!(browse(&mut v).cursor, a + 2, "the cursor follows");
        mouse(&mut v, drag, x, y2 - 1);
        assert_eq!(ticked(&mut v), names(&mut v, a..=a + 1), "back: one off");
        mouse(&mut v, MouseEventKind::Up(MouseButton::Left), x, y2 - 1);
        assert_eq!(mouse(&mut v, drag, x, y2), Outcome::None, "over: no drag");
        // A new drag adds to what is ticked.
        let (y5, y6) = (at(&mut v, a + 5), at(&mut v, a + 6));
        mouse(&mut v, MouseEventKind::Down(MouseButton::Left), x, y5);
        mouse(&mut v, drag, x, y6);
        assert_eq!(ticked(&mut v).len(), 2 + 2);
        mouse(&mut v, MouseEventKind::Up(MouseButton::Left), x, y6);
        // Up past the rows: towards the top, never the shown folder's row.
        browse(&mut v).ticked.clear();
        let y1 = at(&mut v, 1);
        browse(&mut v).top = 0;
        mouse(&mut v, MouseEventKind::Down(MouseButton::Left), x, y1);
        mouse(&mut v, drag, x, 0);
        assert_eq!(ticked(&mut v), ["big"]);
        mouse(&mut v, MouseEventKind::Up(MouseButton::Left), x, 0);
        // Down past the rows of a short pane: the list scrolls as the cursor goes.
        browse(&mut v).ticked.clear();
        text(&mut v, 120, 12);
        let rows = v.hits.rows;
        let top = browse(&mut v).top;
        let last = rows.bottom() - 1;
        mouse(&mut v, MouseEventKind::Down(MouseButton::Left), x, last);
        mouse(&mut v, drag, x, rows.bottom() + 1);
        text(&mut v, 120, 12);
        assert_eq!(browse(&mut v).top, top + 1, "scrolled one row");
        assert_eq!(ticked(&mut v).len(), 2);
    }

    #[test]
    fn ticks_stay_through_the_folder_shown_the_order_and_a_scan_again() {
        let (_t, home) = home("view-tick-stay");
        let cache = home.join(".cache");
        let mut v = ready(policy(&home, None), &home);
        tick(&mut v, &cache.join("big/inner"));
        tick(&mut v, &cache.join("small"));
        select(&mut v, ".cache");
        key(&mut v, KeyCode::Enter);
        key(&mut v, KeyCode::Char('s'));
        assert_eq!(ticked(&mut v), ["inner", "small"]);
        key(&mut v, KeyCode::Char('r'));
        settle(&mut v);
        let b = browse(&mut v);
        let mut paths: Vec<PathBuf> = b.ticked.iter().map(|&i| b.tree.path(i)).collect();
        paths.sort();
        assert_eq!(
            paths,
            [cache.join("big/inner"), cache.join("small")],
            "by path"
        );
    }

    /// The dialog's text at 120x40.
    fn dialog_text(v: &mut View) -> Vec<String> {
        text(v, 120, 40)
    }

    #[test]
    fn del_with_ticks_takes_the_ticked_lists_the_refused_and_refuses_all_without_a_dialog() {
        let (_t, home) = home("view-tick-del");
        let cache = home.join(".cache");
        let mut v = ready(policy(&home, None), &home);
        // Only refused: a message, no dialog.
        select(&mut v, ".config");
        key(&mut v, KeyCode::Char(' '));
        select(&mut v, "Downloads");
        key(&mut v, KeyCode::Delete);
        assert!(browse(&mut v).confirm.is_none());
        let msg = v.message.clone().unwrap();
        // One refusal: .config (protected with all in it) is not opened up.
        let want = "Cannot move to the trash: the ticked item cannot go; protected: ";
        assert!(msg.starts_with(want), "{msg}");
        // The refused one is listed with why.
        tick(&mut v, &cache.join("big"));
        tick(&mut v, &cache.join("small"));
        select(&mut v, "Downloads"); // the cursor's row is not ticked: it does not go
        key(&mut v, KeyCode::Delete);
        let c = browse(&mut v).confirm.as_ref().unwrap();
        let items: Vec<&PathBuf> = c.items.iter().map(|i| &i.1).collect();
        assert_eq!(items, [&cache.join("big"), &cache.join("small")]);
        assert_eq!(c.refused.len(), 1);
        let lines = dialog_text(&mut v);
        assert!(has(&lines, " Move 2 items to the trash? "), "{lines:#?}");
        assert!(has(&lines, &cache.join("big").display().to_string()));
        assert!(has(&lines, "Not moved (1):"));
        let refused = format!("⊘ {}: protected: ", home.join(".config").display());
        assert!(has(&lines, &refused), "{lines:#?}");
        // One ticked item, nothing refused: the dialog for one, saying it is the ticked one.
        key(&mut v, KeyCode::Esc);
        key(&mut v, KeyCode::Esc); // the ticks
        tick(&mut v, &cache.join("small"));
        select(&mut v, ".config");
        key(&mut v, KeyCode::Delete);
        let lines = dialog_text(&mut v);
        assert!(
            has(&lines, " Move the ticked item to the trash? "),
            "{lines:#?}"
        );
        assert!(has(&lines, &cache.join("small").display().to_string()));
        // A ticked folder the guard refuses but whose items may go (Downloads protects only
        // itself): it is not moved, what is in it is, each checked on its own.
        key(&mut v, KeyCode::Esc);
        key(&mut v, KeyCode::Esc);
        tick(&mut v, &home.join("Downloads"));
        key(&mut v, KeyCode::Delete);
        let c = browse(&mut v).confirm.as_ref().unwrap();
        assert_eq!(c.items[0].1, home.join("Downloads/linked"));
        assert_eq!((c.items.len(), c.refused.len()), (1, 1));
        assert_eq!(c.refused[0].0, home.join("Downloads"));
        let lines = dialog_text(&mut v);
        assert!(has(&lines, " Move 1 item to the trash? "), "{lines:#?}");
    }

    #[test]
    fn a_drag_ends_with_any_key_and_with_new_rows() {
        let (_t, home) = home("view-drag-stale");
        let cache = home.join(".cache");
        let mut v = ready(policy(&home, None), &cache);
        let f0 = browse(&mut v)
            .tree
            .find(&cache.join("big/inner/f0"))
            .unwrap();
        browse(&mut v).select(f0);
        text(&mut v, 120, 40);
        let (rows, x) = (v.hits.rows, v.hits.rows.x + 3);
        let press = MouseEventKind::Down(MouseButton::Left);
        let drag = MouseEventKind::Drag(MouseButton::Left);
        let y = |i: u16| rows.y + i;
        // Pressed on a file; a key collapses inner: the rows shrink. A drag then does nothing.
        mouse(&mut v, press, x, y(20));
        select(&mut v, "inner");
        key(&mut v, KeyCode::Left);
        assert!(!row_shown(&mut v, "f0"));
        assert_eq!(mouse(&mut v, drag, x, y(24)), Outcome::None);
        assert!(ticked(&mut v).is_empty());
        // Pressed, then Space ticks another row: a drag does not take that tick away.
        mouse(&mut v, press, x, y(1));
        select(&mut v, "small");
        key(&mut v, KeyCode::Char(' '));
        mouse(&mut v, drag, x, y(2));
        assert_eq!(ticked(&mut v), ["small"]);
        // A release lost (outside the window), then a new order: a stray drag does nothing.
        mouse(&mut v, press, x, y(1));
        key(&mut v, KeyCode::Char('s'));
        assert_eq!(mouse(&mut v, drag, x, y(3)), Outcome::None);
        assert_eq!(ticked(&mut v), ["small"]);
    }

    #[test]
    fn page_down_never_splits_a_path_so_reading_page_by_page_moves_them() {
        let (_t, home) = home("view-tick-pages");
        let many = home.join(".cache/many");
        fs::create_dir(&many).unwrap();
        fs::write(many.join("a"), b"x").unwrap(); // one line
        for i in 0..20 {
            // Two lines each in a 76-cell list.
            let name = format!("item-{i:02}-with-a-name-long-enough-to-take-a-second-line-here");
            fs::write(many.join(name), b"x").unwrap();
        }
        let mut v = ready(policy(&home, None), &many);
        let b = browse(&mut v);
        b.ticked = b.rows[1..].iter().map(|r| r.0).collect();
        key(&mut v, KeyCode::Delete);
        arm(&mut v);
        text(&mut v, 120, 20);
        for _ in 0..10 {
            key(&mut v, KeyCode::PageDown);
            text(&mut v, 120, 20);
        }
        assert!(
            browse(&mut v).confirm.as_ref().unwrap().fits,
            "every path shown whole"
        );
        // End, then more: no overflow.
        key(&mut v, KeyCode::End);
        key(&mut v, KeyCode::Down);
        key(&mut v, KeyCode::PageDown);
        mouse(&mut v, MouseEventKind::ScrollDown, 60, 10);
        text(&mut v, 120, 20);
    }

    #[test]
    fn the_dialog_for_many_moves_them_only_once_every_path_was_shown() {
        let (_t, home) = home("view-tick-many");
        let cache = home.join(".cache");
        let many = cache.join("many");
        fs::create_dir(&many).unwrap();
        for i in 0..30 {
            fs::write(many.join(format!("item-{i:02}")), b"x").unwrap();
        }
        let mut v = ready(policy(&home, None), &many);
        let b = browse(&mut v);
        b.ticked = b.rows[1..].iter().map(|r| r.0).collect();
        key(&mut v, KeyCode::Delete);
        arm(&mut v);
        // 120x20: twelve of the thirty paths at a time.
        let lines = text(&mut v, 120, 20);
        assert!(has(&lines, "lines 1-12 of 30"), "{lines:#?}");
        assert!(!browse(&mut v).confirm.as_ref().unwrap().fits);
        let (_, moving) = v.hits.buttons.unwrap();
        v.last_click = None;
        click(&mut v, moving.x + 2, moving.y);
        assert!(
            many.join("item-00").exists(),
            "not all shown: nothing moves"
        );
        // End jumps to the last ones: those in between were never shown.
        key(&mut v, KeyCode::End);
        assert!(has(&text(&mut v, 120, 20), "lines 19-30 of 30"));
        key(&mut v, KeyCode::Enter);
        assert!(many.join("item-00").exists() && browse(&mut v).confirm.is_some());
        // The wheel and the keys show the rest.
        mouse(&mut v, MouseEventKind::ScrollUp, 60, 10);
        mouse(&mut v, MouseEventKind::ScrollUp, 60, 10);
        text(&mut v, 120, 20);
        key(&mut v, KeyCode::PageUp);
        text(&mut v, 120, 20);
        assert!(
            browse(&mut v).confirm.as_ref().unwrap().fits,
            "every path shown"
        );
        // Too small a window takes the move back.
        text(&mut v, 120, 10);
        assert!(!browse(&mut v).confirm.as_ref().unwrap().fits);
        text(&mut v, 120, 20);
        key(&mut v, KeyCode::Enter);
        assert!((0..30).all(|i| !many.join(format!("item-{i:02}")).exists()));
        assert_eq!(
            v.message.as_deref(),
            Some("Moved 30 items to the trash, 122.9 kB - freed when the trash is emptied")
        );
        assert!(
            matches!(v.screen, Screen::Refreshing(..)),
            "then scanned again"
        );
        assert!(saved(&v).ticked.is_empty() && v.done);
        settle(&mut v);
        assert!(ticked(&mut v).is_empty());
    }

    #[test]
    fn a_confirm_moves_each_through_the_guard_and_the_refused_stay_ticked() {
        let (_t, home) = home("view-tick-move");
        let cache = home.join(".cache");
        let mut v = ready(policy(&home, None), &home);
        select(&mut v, ".config");
        key(&mut v, KeyCode::Char(' '));
        tick(&mut v, &cache.join("small"));
        tick(&mut v, &cache.join("linked"));
        key(&mut v, KeyCode::Delete);
        arm(&mut v);
        text(&mut v, 120, 40);
        // Gone since the dialog opened: the guard refuses it then, and says so.
        fs::rename(cache.join("linked"), home.join("linked-away")).unwrap();
        key(&mut v, KeyCode::Enter);
        assert!(!cache.join("small").exists());
        let msg = v.message.clone().unwrap();
        assert!(
            msg.starts_with("Moved 1 item to the trash, 4.1 kB"),
            "{msg}"
        );
        assert!(msg.contains("; 1 not moved: "), "{msg}");
        settle(&mut v);
        assert_eq!(ticked(&mut v), [".config"], "what did not go stays ticked");
    }

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
