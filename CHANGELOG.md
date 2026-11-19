# Changelog

What changed in each termdisk release, newest first. The version rule is in `AGENTS.md`.

## [0.1.0] - Unreleased

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
