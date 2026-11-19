//! The embeddable view: what is on screen and what keys and clicks do to it. It draws into a
//! ratatui `Buffer` (draw.rs) and takes crossterm events, both only as data: the terminal itself
//! belongs to the host (the CLI in main.rs, or termcode).

use std::collections::{HashMap, HashSet};
use std::mem;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;

use crate::guard::{self, Policy};
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

pub struct View {
    pub(crate) policy: Policy,
    pub(crate) screen: Screen,
    /// Apparent sizes (file lengths) rather than disk usage.
    pub(crate) apparent: bool,
    pub(crate) panes: Panes,
    pub(crate) help: bool,
    pub(crate) message: Option<String>,
    /// Where things were drawn last, for the mouse.
    pub(crate) hits: Hits,
    /// The view is drawn here, the size of its area, then copied out: nothing it draws can
    /// land outside the area it was given (a host's other panes stay untouched).
    pub(crate) canvas: Buffer,
    last_click: Option<(Instant, u16, u16)>,
    /// When a running scan's progress was last asked to be drawn.
    progress_drawn: Instant,
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
    /// Children sorted by size, largest first, trashed ones left out.
    sorted: HashMap<NodeId, Vec<NodeId>>,
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
            hits: Hits::default(),
            canvas: Buffer::empty(Rect::default()),
            last_click: None,
            progress_drawn: Instant::now(),
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
            sorted: HashMap::new(),
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
                b.rebuild(Some(sel));
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
        self.message = Some(match guard::trash(&b.tree, c.id, &self.policy) {
            Ok(t) => {
                b.tree.mark_trashed(c.id);
                b.sorted.clear();
                if c.id == b.zoom {
                    b.zoom = b.tree.parent(c.id).unwrap_or(0);
                }
                let at = b.cursor;
                b.rebuild(None);
                b.cursor = at.min(b.rows.len() - 1);
                format!(
                    "Moved to the trash: {} (the file manager can put it back; the space is freed when the trash is emptied)",
                    t.from.display()
                )
            }
            Err(r) => format!("Not moved: {r}"),
        });
    }

    pub fn handle_mouse(&mut self, m: MouseEvent) -> Outcome {
        let (x, y) = (m.column, m.row);
        let inside = |r: Rect| x >= r.x && x < r.right() && y >= r.y && y < r.bottom();
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {}
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let up = m.kind == MouseEventKind::ScrollUp;
                if let Screen::Browse(b) = &mut self.screen {
                    if b.confirm.is_none() && inside(self.hits.rows) {
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
        self.last_click = Some((now, x, y));
        if self.help {
            self.help = false;
            return Outcome::Redraw;
        }
        self.message = None;
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
                let ok = b.confirm.as_ref().is_some_and(|c| c.armed() && c.fits);
                if inside(cancel) {
                    b.confirm = None;
                } else if inside(moving) && ok {
                    self.do_trash();
                } else {
                    return Outcome::None;
                }
            }
            Screen::Browse(b) if inside(self.hits.rows) => {
                let row = b.top + usize::from(y - self.hits.rows.y);
                if row < b.rows.len() {
                    b.cursor = row;
                    if double {
                        b.zoom_in(b.rows[row].0);
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
                let kids = self.kids(id).to_vec();
                stack.extend(kids.into_iter().rev().map(|k| (k, depth + 1)));
            }
        }
        self.rows = rows;
        if let Some(k) = keep {
            if let Some(i) = self.rows.iter().position(|r| r.0 == k) {
                self.cursor = i;
            }
        }
        self.cursor = self.cursor.min(self.rows.len() - 1);
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

    /// The child of `zoom` that `id` is or lies under (its tile in the treemap).
    pub fn tile_of(&self, mut id: NodeId) -> Option<NodeId> {
        loop {
            let p = self.tree.parent(id)?;
            if p == self.zoom {
                return Some(id);
            }
            id = p;
        }
    }
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
        assert!(has(&lines, ".config/ 🔒"));
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
        assert!(has(&text(&mut v, 120, 40), "linked (hard link)"));
        key(&mut v, KeyCode::Delete);
        assert!(has(&text(&mut v, 120, 40), "may free nothing"));
    }

    #[test]
    fn tree_and_map_select_each_other_and_double_click_zooms() {
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
        assert_eq!(b.tile_of(big), Some(big), "the row selects its tile");
        // Double click on the "big" row: its contents fill the view.
        text(&mut v, 120, 40);
        let row = v.hits.rows;
        let b = browse(&mut v);
        let y = row.y + (b.cursor - b.top) as u16;
        click(&mut v, row.x + 3, y);
        click(&mut v, row.x + 3, y);
        assert_eq!(browse(&mut v).zoom, big);
        assert_eq!(selected_name(&mut v), "inner");
        key(&mut v, KeyCode::Backspace);
        assert_eq!(browse(&mut v).zoom, 0);
        assert_eq!(selected_name(&mut v), "big");
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
}
