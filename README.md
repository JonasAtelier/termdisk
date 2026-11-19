# termdisk

A disk usage analyzer for the terminal, in pure Rust: the terminal counterpart of GNOME's
Disk Usage Analyzer (baobab), with its treemap, made for finding what is big and clearing
caches and temporary files.

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
| v | tree and map / tree / map |
| r | scan again |
| ? | all the keys |
| q | quit |

In the dialog for Del, Cancel has the focus, so Enter alone cancels; Tab (or an arrow) and
Enter, or a click, moves the item. For half a second after the dialog opens, only Cancel
works. Items that cannot be trashed show a 🔒, and Del on one says why.

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

## Build

```
cargo build --release   # Rust 1.93 or newer
```
