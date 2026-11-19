# Changelog

What changed in each termdisk release, newest first. The version rule is in `AGENTS.md`.

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
