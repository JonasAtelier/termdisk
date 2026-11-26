# Changelog

What changed in each termdisk release, newest first. The version rule is in `AGENTS.md`.

## [0.4.1] - 2026-11-26

### Changed
- README: an animated wordmark and demos of the treemap, navigation and drag to Trash, and a
  quick install guide. termdisk itself is as in 0.4.0.

## [0.4.0] - 2026-11-20

### Added
- Ticks: Space ticks or unticks the selected row and goes on to the next; a drag with the left
  button over the rows ticks them (dragging back takes the ticks off again; past the top or
  bottom the list scrolls). The shown folder's own row takes no tick. Every other row has a
  tick box in its icon's place, files and folders alike (`☐` and `☑`; `[ ]` and `[x]` in
  ASCII; the file icon `▪` gives way to it): a click on it ticks or unticks the row and puts
  the cursor there (two quick clicks are two ticks, never a double click), the pointer on it
  underlines it, and a drag from it gives the rows it goes over the state it took. A ticked
  folder ticks everything in it (each row shows `☑`); taking the tick off one item in it takes
  off only that item: the folder's tick goes, and each other item on the way down is ticked on
  its own (a drag that takes ticks off does the same; one that puts them on changes nothing
  in a ticked folder). A folder that is not ticked but holds a ticked item shows `◩` (`[-]`
  in ASCII), even when all in it is ticked, since the folder itself would not go; a click
  there ticks the folder, and the ticks in it become part of it. A ticked row has a tint
  too, and the status bar says how many are ticked on their own (as the dialog lists them;
  a ticked folder is one) and how big.
  Ticks stay when another folder is shown or the order changes, and after a scan again (by
  path). Esc clears them.
- Del with ticks moves the ticked items, not the selected row; what shows ticked is what may
  go. Each is checked first, and a ticked folder that passes goes whole. One that cannot go
  is listed in the dialog with why and stays; if what is in it may still go (the folder
  protects only itself, as `~/.cache` and Downloads, or the refusal is about something in it,
  such as a locked item), each item in it is checked on its own, and so on down. A folder
  that cannot go with all in it (`~/.config`, `~/.local`, not yours, a mount point) and a home
  are not opened up. Those that cannot go stay ticked, a folder opened up this way too, once
  what was in it has gone; if none can go, the status bar says why and no dialog opens.
  The dialog for several items shows how many and how big, and every path, whole, in a list
  that scrolls (↑ ↓ PgUp PgDn Home End, the wheel; PgDn never splits a path between two
  pages); Move to Trash works only once every path to move has been shown whole (what End
  jumps over does not count), with the same waits as for one item. Each item then goes
  through every check again, one at a time; the status bar says how many went and how big,
  and what did not. One ticked item has the dialog for one, titled "Move the ticked item to
  the trash?". A key while the mouse button is held, or new rows, end a drag.
- Emptying the trash, for good: Del on the start screen's Trash scans the trash, then a dialog
  says how many items and how much disk will be deleted permanently and that this cannot be
  undone, with the trash dialog's focus and waits (Empty Trash in place of Move to Trash);
  Del on the trash folder's own row while browsing it does the same. Each item in `files`
  goes, then its `.trashinfo`, then `directorysizes`; the trash folders stay. It runs on a
  thread of its own under a card (Esc stops it: the item it is at stays in the trash, partly
  deleted; keys and clicks wait),
  then the status bar says how many items went, how much was freed, and what could not go and
  why, and the trash is scanned again. Only through folders held open, never through a
  symbolic link (one in the trash is removed itself) or into a mount point or another
  filesystem, only what is yours, only a trash in your home whose folders are real folders of
  yours (`files` and `info` no mount point); it changes no permissions, so what a
  read-only folder in the trash holds stays and is reported. An empty trash: a message, no
  dialog. Not with `--read-only`.
- Library: `Colors` has two more fields, `tick_bg` and `tick_fg` (the tint of a ticked row,
  and its ticked box): a host that builds `Colors` field by field must add them. A host gives
  the view the left button's drags and releases as well as its clicks. In the icon sets the
  tick boxes (`boxes`: not ticked, ticked, holding a ticked item) replaced the tick mark and
  the file icon.

### Changed
- The title bar ends with termdisk's version ("termdisk v0.4.0") on every screen. Where the
  title has no room, the path is cut in its middle first, then the size goes; the version
  stays. On another filesystem than the trash, the "look only" note gives way before the
  size.
- The tree's names get the room the columns do not use: Size and Items are as wide as their
  values on screen, and Usage's bar gives up to 5 of its 8 cells while names are cut. Which
  columns show is decided as before (Items goes first in a narrow pane; the sorted column
  stays), so it does not change as the rows scroll.
- The map's tiles show their text in bold, on every tile.
- The lock shows only on what cannot be trashed itself: a protected path or what lies inside a
  wholly protected one, what is not yours, a mount point, a folder that could not be read. A
  folder that only holds such an item has no lock (it still cannot be trashed); Del on it says
  how many locked items it holds and the first one the check met. With the pointer on a lock,
  in the tree or on a tile, a box says why ("protected: ~/.config, with everything in it",
  "not yours: another user owns it", "a mount point: …", "could not be read"), from what the
  scan saw: drawing it never waits on the disk.
- After a move to the trash the folder is scanned again at once, and `r` does the same. The
  last tree is let go as the scan starts, so two trees never share the memory: the scan's card
  shows meanwhile, under the folder's path, with the message of what the trash did (keys
  wait; Esc stops it and goes back to the places, as for a first scan). Then the new tree
  shows as the last one was: the same folder, folders open, ticks, item selected on
  the same line (or the nearest folder above it still there), all kept by path. Where the
  trash lies inside the scanned folder (a home), the item now shows there, in
  `.local/share/Trash`, instead of leaving the total. A folder that cannot be scanned again
  keeps its tree, and the message says why.
- The map is laid out in strips: rows of tiles as wide as the map, from top to bottom, each
  filled left to right largest first (an ordered strip treemap, Bederson, Shneiderman and
  Wattenberg), so the tiles read like text, the tiles of a row share their top and bottom,
  and items of one size and name length get tiles of one width. Every tile shows its size and
  its share of the folder ("7.3 MB  4%", as the Usage column), so areas follow the sizes
  compressed (`size ^ 0.6`): a folder whose largest item is a million times its smallest still
  shows many of them, the larger ones larger. Nothing on a tile is cut: an item has a tile of
  its own only where its class letter, whole name, size and share fit inside the frame (and 4
  rows); the others, also an item whose name is wider than the map or that no strip has room
  for beside the others (the items after it keep their tiles, for up to 8 such items), share
  the last tile, "+N items", which lists them inside its frame, largest first, a name, its
  size and share a line,
  each line whole or told in "+N more"; too narrow for its label it shows "+N". The strips
  break where that gives the most items a tile of their own, then the squarest tiles nearest
  their areas; each strip is its height in whole rows, rounded down or up so that every strip,
  the last too, is within a row of its exact height. In a strip each tile gets its text's
  columns and the room left goes by area; the texts of small items may squeeze a larger one,
  never under half its area, so a small item with a long name can be drawn larger than a
  larger neighbour (the size and share on each tile are right). The gap under a row of tiles
  is half a row in the Nerd and Unicode sets (`▀` in the tile's colour), as wide to the eye as
  the column between tiles; ASCII keeps a whole row. The rest's tile has a frame from 2 rows
  up. On a 300x80 screen the layout takes 0.6 ms for 400 files of one size and
  1.4 ms for a folder of 166 000 files, both with the narrowest texts a tile can have
  (names make them fewer a row); the whole view with that folder shown, 1.3 to 1.4 ms.
- A scan again through a symlink put where the folder was keeps no ticks: a path below the
  folder may now be something else, and ticks pick what Del moves.
- Memory: the tree takes 58 bytes per entry (was 73): a node is 40 bytes (was 56). On a home
  of 1.24 million entries, `--summary` peaks at 87 MB (was 106) and a scan again at 88 MB
  (was 211 to 222: the last tree was kept until the new one was shown). The program fixes glibc's
  limit for mapping large blocks at 128 KiB (glibc raised it as trees were freed, and a scan
  again then grew the next tree inside the heap: 40 MB more), and after each scan hands the
  memory freed back to the system (`malloc_trim`; a host with glibc's defaults gains that
  part).
- The scan uses up to 12 threads (was 8): 1.24 million entries warm in 0.26 s (was 0.30, the
  time `--summary` prints).
- Library: `tree::NOT_OWNED`, `UNREADABLE`, `MOUNT` and `PROTECTED` now mark the item itself
  only; the new `tree::HOLDS` marks the folders above one, and `tree::LOCKED` is the four.
- Library: `Node` is 40 bytes. `Node::items` is gone: `Tree::items(id)` counts what lies under
  a folder (0 for a file). `Node::mtime` is a `u32` of seconds since the epoch (was `i64`): 0
  before 1970, `u32::MAX` after 2106. `treemap::layout` takes a third argument, `cols(item)`:
  the columns the tile of `item` needs for its whole text, frame and gutter; its tiles are
  strips, every `Item::One` tile at least that wide and 4 rows high, and a single item without
  a tile is `Item::Rest(1)` (it was `Item::One` of its own).

### Fixed
- The trash dialog: Tab, Shift+Tab and the arrows move the focus at once (they did nothing for
  the first half second, so a quick Del, Tab, Enter pressed Cancel). Nothing can move any
  sooner: Enter on Move to Trash does nothing, and the dialog stays, until the half second is
  over and the focus has been there a moment; meanwhile the focused button is drawn in the
  focus's colours, crossed out.
- A home without `~/.local/share` could not trash anything ("the trash cannot be used: No such
  file or directory"). The missing folders are made now, mode 0700, as the first item goes
  (not when the dialog opens: `check` makes nothing), only inside your home and in a folder of
  yours, none through a symbolic link; the start screen and the title no longer call such a
  home "look only". Folders above the trash that are symbolic links (`/home` on some systems,
  your own `~/.local`) work as before.

## [0.3.0] - 2026-11-19

### Added
- A red cross at the right of the title bar, on every screen: a click on it does what `q`
  does (quits; nothing under the trash dialog). Under the pointer it turns red. Library:
  `Colors::close_fg`, `Colors::close_hover`.

### Changed
- The tree sorts by name, size, items or last change, either way round: a click on a column
  heading (Usage sorts by size; again: the other way), or `s` (the next column) and `S` (the
  other way). Names go A to Z with capitals and small letters together; the other columns
  start largest or newest first. The heading sorted by has an arrow. The default is still
  size, largest first, and the selection stays on its item.
- The tree's columns are parted by faint lines (`Colors::column_line`), and a heading under the
  pointer underlines its text only, the same for every column.
- In the tree, folders come first in every folder, then the files and the rest, each in the
  sorted order (turning the order round keeps the folders on top).
- The tree's levels have colours: each level of the indent has a guide line (`│`, or `|` in
  ASCII), and the guides, chevrons and folder icons take soft colours by
  level in turn (gold, orchid, steel blue; fitted to 256 and 16 colours, neighbouring levels
  differing). The selected row keeps its own colours. Library: `Colors::depth`.
- The Usage column shows the share of its folder as a percentage before the bar ("41%";
  "<1%" below half a percent, ">99%" for nearly all, "100%" only for all); the bar is 8 cells.
  A narrow tree keeps the column it is sorted by.
- The map follows the selection: it shows the folder that holds the selected row, that row's
  tile selected, open folder or not; on the shown folder's own row, that folder. Every tile
  with room is framed (the selected one in white, with the mark in its corner). Its
  legend starts with that folder's name. The map stays largest first whatever the tree's order.
- The map's tiles take 8 soft colours in turn (in 16 colours they keep their hue), so
  neighbouring tiles differ, and only the selected tile is blue, also in 256 and 16 colours (the "+N items" tile when the selected item is among
  them). The tiles keep their type or age letters (`t` now switches only those), and the
  legend lists the letters without colours.
- Library: `Colors::kinds` and `Colors::ages` are replaced by `Colors::tiles` and
  `Colors::sel_tile`.

## [0.2.2] - 2026-11-19

### Changed
- Releases reach crates.io through its Trusted Publishing: the release job, once the owner
  approves, gets a token from crates.io for 30 minutes (GitHub's OIDC, this repository's
  `release.yml` in its `crates-io` environment), so no token is stored any more. termdisk
  itself is as in 0.2.1.

## [0.2.1] - 2026-11-19

### Changed
- The licence: from this version termdisk is GPL-3.0-only (0.2.0 and earlier: MIT).
- The crate is ready for crates.io: its description, keywords and categories, and only the
  code, tests, examples and documents in the package.
- CI on GitHub Actions: format, lint, tests, dependency licences, the package and the oldest
  supported Rust on every push to main and every pull request; a version tag publishes on
  crates.io and makes the GitHub Release, after the owner approves.

## [0.2.0] - 2026-11-19

### Changed
- The tree: a double click on a folder opens it where it is, its contents indented under it;
  another double click closes it (folders start closed, as before). It no longer shows that
  folder: Enter, or a double click on its tile in the treemap, still does. A double click on a
  file, or on the folder shown, does nothing; a third quick click is a single click. When a
  folder closes around the selection, the selection moves up to it.
- The look is termcode's: its colours (VS Code's dark theme: the grey title bar with the path
  in the middle, the blue status bar, the side-bar grey tree with its column headings, the
  blue selection), its icons (Nerd Font codicons, Unicode or ASCII, each one cell; the lock is
  now one cell wide too), thin usage bars, and its rounded popups for the trash dialog and the
  keys, the button with the focus in blue. A Move to Trash that cannot be used yet is dim and
  crossed out. In 16 colours or ASCII the selection keeps a mark at its edge and stays bold.
  `TERMDISK_COLORS` and `TERMDISK_ICONS` override what is detected. Library: `Colors`,
  `Depth`, `IconSet`, `View::set_colors`, `View::set_icons`.
- The map is coloured by type (from the name), or by age with the new `t` key, under a
  one-line legend; every tile starts with its class letter. The selected tile has an outline.
- The pointer lights up rows, places, tiles, the dialog's buttons and the status bar's items
  (which can now be clicked: `a`, `t`, `?`). The tree has a scrollbar.
- A running scan shows a card with a spinner, the rate and the time taken.
- After a move to the trash the status bar says what went, how many items and how big, with a
  check mark, until the next key or click.
- An `fc-list` that hangs is killed after 2 s, or when termdisk ends.

## [0.1.0] - 2026-11-18 (not tagged)

### Added
- `termdisk [PATH]`: the view. Without PATH a start screen offers home, the cache, /tmp and
  the trash, with the home filesystem's use; with PATH it scans PATH at once. Left, the
  folder tree (name, usage bar, size, items, last change; largest first, folders open and
  close); right, a treemap of the folder shown, small items merged into one tile; selecting
  in one selects in the other. Enter or a double click shows a folder, Backspace the one
  above; `a` disk usage or apparent size, `v` both panes or one, `r` scan again, `o` open,
  `c` copy the path, `?` keys, `q` quit; mouse clicks and the wheel. Del moves the selected
  item to the trash after a dialog (path, size, items; Cancel has the focus, only Cancel for
  half a second); items that cannot go show a 🔒 and Del says why. `--read-only` removes
  Del. The terminal is put back on quit, panic, SIGINT, SIGTERM and SIGHUP; nothing runs
  while nothing happens. Library: `View` (`tick`, `next_wake`, `handle_key`, `handle_mouse`,
  `draw` into a ratatui `Buffer`, `Outcome`), `treemap::layout`, `check`.
- `termdisk --summary [PATH]`: scans PATH and prints its disk usage, apparent size, item and
  folder counts, what could not be read, the time taken and the memory the tree takes.
  Symbolic links are never followed, automounts are not triggered, other filesystems and
  bind mounts are listed but not entered, and hard-linked files count once. On a home folder
  of 2 million entries: 0.4 s and 154 MB, against 2.3–2.9 s for ncdu 1.22 and du, and 222 MB
  for ncdu. Library: `termdisk::scan` (dropping the `Scan` cancels it), `Progress`, `Tree`,
  `Node`.
- Library: `termdisk::trash` moves one scanned item to the home trash (it can be put back
  from the file manager; no space is freed until the trash is emptied). It refuses, checking
  the disk at that moment: anything when running as root, setuid or with the power to change
  others' files; items that are not wholly the user's; protected paths, for both `$HOME` and
  the home in the user database (the scan root; home and the folders above it; whole:
  `.config`, `.local`, `.var`, `snap`, browser and mail profiles, keys, credentials, shell
  setup and history, `.claude`, synced folders, Desktop, Documents, Pictures, Music, and paths
  listed in `~/.config/termdisk/protect`; themselves only: `.cache`, Downloads, Videos,
  Templates, Public); anything with a mount point or an unreadable folder in it; anything
  changed since the scan; anything not on the home trash's filesystem. Nothing is ever deleted
  or copied. It only reads the tree (`Tree::mark_trashed` marks it afterwards). `Policy`,
  `Refusal`, `Trashed`.
