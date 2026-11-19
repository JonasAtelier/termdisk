# termdisk

A disk usage analyzer for the terminal, in pure Rust: the terminal counterpart of GNOME's
Disk Usage Analyzer (baobab), with its treemap, made for finding what is big and clearing
caches and temporary files.

```
                 ~/Downloads  —  4.4 MB (disk usage)  —  termdisk
 Name                       Usage      Size   Modified   V video I image A audio Z archive D doc T text O other
 ▾ Downloads/ ⊘        ━━━━━━━━━━    4.4 MB 2026-11-19  ╭Z linux.iso────────────────────────────────────────╮
▌    ▪ linux.iso       ━━━━──────    1.8 MB 2025-10-15  │1.8 MB                                             │
     ▪ clip.mkv        ━━────────  901.1 kB 2026-11-17  │                                                   │
     ▪ photo.jpg       ━╸────────  602.1 kB 2026-11-04  ╰───────────────────────────────────────────────────╯
     ▪ song.mp3        ━─────────  450.6 kB 2026-08-11   V clip.mkv                   A song.mp3
     ▪ notes.pdf       ╸─────────  303.1 kB 2024-09-10   901.1 kB                     450.6 kB
     ▪ script.py       ╸─────────  122.9 kB 2026-11-18   I photo.jpg                  D notes.pdf   T scrip…
 ↑↓ move  →← open  Enter zoom  ⌫ up  Del trash  v panes  r rescan  q quit       a: disk usage  t: by type  ?: keys
```

(A sketch of the view without its colours.)

- Fast: a home folder of 2 million entries scans in about half a second (see Speed).
- A folder tree and a treemap side by side, keyboard and mouse.
- Careful: it only ever moves things to the trash, and refuses what you would miss
  (see what it refuses, below).
- A library without terminal code, so another program can embed the view.

## Install

```
cargo install --git https://github.com/JonasAtelier/termdisk --locked
```

Needs Linux and Rust 1.93 or newer. Moving to the trash needs Linux 5.8 or newer (on an
older kernel termdisk only looks). A Nerd Font and a mouse are used when there, not needed.

## Use

```
termdisk                    choose a place: home, cache, /tmp or the trash
termdisk PATH               scan PATH and show it
termdisk --read-only PATH   the same, with no way to move anything to the trash
termdisk --summary [PATH]   scan PATH (default: the current folder) and print its totals
```

The left pane is the folder tree (name, usage bar, size, items, last change), largest first;
the right pane is a treemap of the folder shown. Selecting in one selects in the other.
Folders in the tree start closed.

| Key | |
|---|---|
| ↑ ↓ PgUp PgDn Home End | move |
| → ← (or + -) | open, close a folder |
| double click on a folder in the tree | open it where it is; again: close it |
| Enter, double click on a tile | show that folder |
| Backspace | show the folder above |
| Del | move to the trash (asks first) |
| o | open with the default program |
| c | copy the path (OSC 52) |
| a | disk usage / apparent size |
| t | map colours: by type / by age |
| v | tree and map / tree / map |
| r | scan again |
| ? | all the keys |
| q | quit |

In the dialog for Del, Cancel has the focus, so Enter alone cancels; Tab (or an arrow) and
Enter, or a click, moves the item. For half a second after the dialog opens, only Cancel
works. Items that cannot be trashed show a lock (`⊘`, or `!` in ASCII), and Del on one says
why. After a move the status bar says what went and how big it was until the next key or
click, which also does what it always does.

The map is coloured by type (from the name only) or, with `t`, by the age of the last
change; each tile starts with its class letter, explained in the legend above the map
(`/` folder, `V` video, `I` image, `A` audio, `Z` archive, `D` document, `T` code and text,
`O` other; `W` this week, `M` this month, `Y` this year, `O` older), so colour is not needed
to read it. The items at the right of the status bar (`a`, `t`, `?`) can be clicked.

The scan reads folders on several threads. It never follows symbolic links (only the path
given is resolved, as `cd` would), stays on PATH's filesystem (mount points under it, bind
mounts too, are listed but not entered), counts hard-linked files once, and gives both disk
usage (allocated blocks, as `du`) and apparent size (file lengths, as `du -b`).

termdisk never deletes anything: it moves one item at a time to your trash
(`~/.local/share/Trash`), where the file manager can put it back. It refuses, whole with
everything in them: `~/.config`, `~/.local`, `~/.var`, `~/snap`, browser and mail profiles
(`.mozilla`, `.thunderbird`), keys and credentials (`.ssh`, `.gnupg`, `.aws`, `.kube`,
`.docker`, `.netrc`, `.git-credentials`, ...), shell setup and history, `.claude`, synced
folders (`Dropbox`, `OneDrive`, `Nextcloud`, ...), and Desktop, Documents, Pictures and Music.
`~/.cache`, Downloads and Videos are protected themselves only: what is in them may go. It
also refuses anything not wholly yours, anything with a mount point in it, and anything on
another filesystem than the trash (where `/tmp` is a tmpfs, that includes `/tmp`). To protect
more, list paths in `~/.config/termdisk/protect`, one per line (absolute or `~/...`). It does
nothing at all when run as root or setuid.

## Speed

Measured on one 12-thread machine with a warm cache, on a home folder of 2.05 million
entries (`scripts/bench.sh` repeats it):

| | Time | Max memory |
|---|---|---|
| `termdisk --summary` | 0.47 s | 155 MB |
| `ncdu -x -0 -o /dev/null` (1.22) | 2.50 s | — (it keeps no tree) |
| `du -sx` | 3.31 s | 209 MB |

The view is up when the scan ends, draws a 300×80 screen in under 0.4 ms, and does nothing
at all while nothing happens. With a cold cache the disk sets the pace: the same scan takes
seconds.

## Limits

- Linux only.
- No permanent delete and no "empty the trash": free the space from the file manager or
  with `gio trash --empty`.
- What is on another filesystem than your trash can be looked at, not moved (`/tmp` where it
  is a tmpfs, other disks).
- Caches kept under `~/.config`, `~/.local` and `~/.var` (Electron and Flatpak programs) are
  inside protected folders and cannot be moved.
- The treemap shows one level: the folder shown.

## Look

termdisk looks like termcode: its default colours (VS Code's dark theme) and its three icon
sets. The colours are fitted to what the terminal shows, from `COLORTERM` and `TERM`; set
`TERMDISK_COLORS=truecolor`, `256` or `16` where that guess is wrong (tmux without true
colour, some ssh sessions). The icons are a Nerd Font's when one is installed (asked of
fontconfig as termcode does; Unicode over ssh); set `TERMDISK_ICONS=nerd`, `unicode` or
`ascii`. A terminal that draws East Asian Ambiguous characters two cells wide (a CJK locale
setting in some terminals) misplaces the Unicode set's arrows, bars and box lines: use
`TERMDISK_ICONS=ascii` there. The selection, the lock on what cannot be trashed and the
dialog's focus also show without colour: a mark at the row's edge, a lock sign, bold and
crossed-out buttons.

## Third-party

The default colours are VS Code's (Copyright (c) Microsoft Corporation, MIT License; the full
text is in `THIRD-PARTY-LICENSES`). Colour fitting and the icon sets are adapted from
termcode (MIT, the same author).

## Build

```
cargo build --release   # Rust 1.93 or newer
cargo test
```

`AGENTS.md` describes the source files, the checks and the version rule; `CHANGELOG.md` what
changed in each version.

## License

© 2026 JonasAtelier, GPL-3.0-only, see `LICENSE`.

termdisk is free software: you can redistribute it and/or modify it under the terms of the
GNU General Public License version 3 as published by the Free Software Foundation (that
version only). It is distributed in the hope that it will be useful, but WITHOUT ANY
WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR
PURPOSE. See the GNU General Public License in `LICENSE` for more details.

Versions up to and including 0.2.0 were released under the MIT license.
