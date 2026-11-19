# Repository Guidelines

`termdisk` is a pure-Rust disk usage analyzer for the terminal (Rust 2021, rust-version
1.93): the terminal counterpart of GNOME's Disk Usage Analyzer (baobab), with its treemap. A
library and a CLI in one crate. The library has no terminal code; keep it that way (termcode
embeds it).

- `src/lib.rs`: the public API, `fmt_size`, and `TempDir`, the temporary folder every test
  that writes files works in.
- `src/tree.rs`: the scan result: an arena of 56-byte `Node`s (a folder's children stored
  together), one buffer of names, flags (kind, not owned, unreadable, mount, hard link,
  protected, trashed), totals added up in one backward pass.
- `src/sys.rs`: the Linux calls the scan and the guard share: folders opened as descriptors
  (never through a symlink), listed with `getdents64`, entries lstat'ed with `statx` (no
  automount, mount roots seen). Raw syscalls, so a static musl build needs no wrapper.
- `src/scan.rs`: the walk: up to 8 threads read folders through `sys`, one thread builds the
  tree; progress, cancel (dropping a `Scan` cancels it). Stays on one filesystem and out of
  mount points (bind mounts too); hard-linked files count once.
- `src/guard.rs`: the only way termdisk removes anything: `trash` (its rules are listed at
  the top of the file, each with its tests) and the `Policy` of protected paths.
- `src/trash.rs`: the home trash (freedesktop.org spec): `.trashinfo`, then one
  `renameat2(RENAME_NOREPLACE)`; reachable only from `guard::trash`.
- `src/treemap.rs`: the squarified treemap in cells (pure; tiles cover the area once).
- `src/view.rs`: the embeddable `View`: screens (start, scanning, browse), the dialog, keys and
  mouse, `tick`/`next_wake` (when nothing waits, the host may block on input). Its tests drive
  it with synthetic events into a `Buffer`.
- `src/draw.rs`: the view drawn into a ratatui `Buffer` (cells only).
- `src/style.rs`: `Colors` (termcode's defaults, `fitted` to 256 or 16 colours once) and
  `IconSet` (termcode's Nerd, Unicode and ASCII glyphs, each one cell). Plain data; a host
  passes its own. Keep every non-colour cue (the selection's edge mark and bold, the lock,
  bold and crossed-out buttons, the tiles' class letters and the selected tile's outline): in
  16 colours the selection is as black as the pane. Hover is styling only: `View::hover_at`
  looks where a click looks, and Move to Trash is drawn disabled exactly when a click on it
  would do nothing (`Confirm::clickable`), hovered or not.
- `src/main.rs`: the CLI: the terminal (raw mode, alternate screen, mouse, bracketed paste so
  pasted text never arrives as keys), put back on quit, error, panic and SIGINT/SIGTERM/SIGHUP
  (a `sigwait` thread); `xdg-open`; OSC 52; `--summary`.
- `tests/bwrap.rs` and the guard's `rule1_and_5_real_mounts_in_a_sandbox`: real mounts made
  in a bwrap sandbox (skipped where bwrap cannot run).
- `scripts/smoke.py BINARY STEPS [--read-only] [PATH]`: the view in a pty with a temporary
  HOME and fixture (keys, waits, thread count, wake-ups, exit code, terminal put back, what
  reached the temporary trash). It refuses a PATH outside its temporary folder without
  `--read-only`. Drive any change to the view through it and report what it prints.
- `scripts/bench.sh`, `examples/frame.rs`: the speed and memory benchmark (scan, view, frame
  times, a trash).

termdisk never deletes: it moves one item at a time to the home trash, after the checks in
`src/guard.rs`. Keep `guard::trash` the single entry point; never add a fallback (copying,
unlinking) when the rename fails, and no permanent delete (the user's decision). The
protected lists are at the top of `src/guard.rs`; `Policy::detect` is the only place that
reads HOME, XDG_* and the passwd entry, so an embedding program cannot hand termdisk a home.

## Build and test

Check `free -m` before builds (this machine has 16 GB and uses swap); size `-j` to available
memory, never more than 4. No parallel cargo runs.

- `cargo build -j4`, `cargo test -j4`
- `cargo clippy -j4 --all-targets -- -D warnings`, `cargo fmt --check`
- Dependencies use the versions termcode has, so embedding adds no crate: every crate and
  version in `cargo tree -e normal` is in termcode's `Cargo.lock` (pin with `cargo update -p
  NAME --precise VERSION`); the one duplicate, syn 2 and 3, is ratatui's and termcode has it too.
- `scripts/bench.sh [BINARY] [DIR]`: wall and max RSS (medians of 5) of `du -sx`, ncdu and
  `termdisk --summary` on DIR (default `$HOME`), the totals against `du` and `find`, bytes per
  entry. Run it before and after any change to the scan or the tree (`cargo build -j4
  --release` first; keep a copy of the old binary) and put both tables in the report.
- Tests never touch real paths: they write only inside a `TempDir` (and one folder under
  `target/`), and every trash test gives the `Policy` a home inside it. Lines in `src/` that
  remove folders or change permissions are test-only and marked so.
- Checks to keep true: `grep -c SYS_renameat2 src/*.rs` finds one (trash.rs), `unlinkat` is
  only the rollback of a `.trashinfo` made moments before (trash.rs), and
  `grep -nE 'remove_dir_all|remove_dir\(|set_permissions|chmod|chown|pkexec|sudo' src/*.rs`
  finds only lines marked test-only.

## Versions

Changes worth telling users go under the next version in `CHANGELOG.md`. Version `X.Y.Z`: X
for a new feature or a huge change (a new chart or view), Y for a change to an existing part
(the tree, the treemap, the scan, a CLI flag), Z for bug fixes. A release bumps only the
largest kind it holds, the lower numbers back to 0 (1.4.2 with a fix and a treemap change:
1.5.0).
