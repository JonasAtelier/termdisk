# Repository Guidelines

`termdisk` is a pure-Rust disk usage analyzer for the terminal (Rust 2021, rust-version
1.93): the terminal counterpart of GNOME's Disk Usage Analyzer (baobab), with its treemap. A
library and a CLI in one crate. The library has no terminal code; keep it that way (termcode
embeds it).

Licence: GPL-3.0-only (`LICENSE`, `license` in `Cargo.toml`; 0.2.0 and earlier were MIT).
Contributions are accepted under it. Each source file starts with its SPDX line.

- `src/lib.rs`: the public API, `fmt_size`, and `TempDir`, the temporary folder every test
  that writes files works in.
- `src/tree.rs`: the scan result: an arena of 40-byte `Node`s (a folder's children stored
  together), one buffer of names in node order (a name ends where the next starts), flags
  (kind, not owned, unreadable, mount, hard link, protected, trashed: each of the item itself;
  "holds" on the folders above a locked one), totals added up in one backward pass. What only
  folders have (children, item count) is a `Folder` each, found by counting the folders
  before a node (a bit per node, a count per 64). Keep `ino` a whole u64: the guard compares
  it with the disk. `scripts/bench.sh` prints bytes per entry; keep it in view.
- `src/sys.rs`: the Linux calls the scan and the guard share: folders opened as descriptors
  (never through a symlink), listed with `getdents64`, entries lstat'ed with `statx` (no
  automount, mount roots seen). Raw syscalls, so a static musl build needs no wrapper.
- `src/scan.rs`: the walk: up to 12 threads read folders through `sys`, one thread builds the
  tree; progress, cancel (dropping a `Scan` cancels it). Stays on one filesystem and out of
  mount points (bind mounts too); hard-linked files count once. After a scan the memory
  freed goes back to the system (`sys::give_back_free_memory`, glibc only).
- `src/guard.rs`: the only way termdisk removes anything: `trash` (its rules are listed at
  the top of the file, each with its tests), `empty_trash` (rule 13) and the `Policy` of
  protected paths.
- `src/trash.rs`: the home trash (freedesktop.org spec): `find` opens it (what is missing is
  made, 0700, only by `trash`, only inside the home); `.trashinfo`, then one
  `renameat2(RENAME_NOREPLACE)`, reachable only from `guard::trash`; `empty`, the only
  deleting, reachable only from `guard::ToEmpty::run`.
- `src/treemap.rs`: the ordered strip treemap in cells (pure; tiles cover the area once, in
  strips read like text; areas follow `size ^ COMPRESS`; every tile of its own as wide as its
  whole text, which the caller measures (`layout`'s `cols`: `draw::map_pane` passes the name,
  size and share), and 4 rows high; the rest in one last tile, which lists what it holds where
  it has room). Each strip is its exact height in whole rows, rounded down or up, the last the
  rows left, every one within a row of exact; within a strip the room beyond the texts goes by
  area, and no tile is drawn under `LEAST_SHARE` of its area. The strip breaks and the
  roundings come from a dynamic programme over those cells as drawn (`examples/frame.rs`
  times its hardest case): the most tiles of their own, then the best fit. Its tests check
  the count against trying every cut, that one more file of a size costs at most a strip of
  tiles, and that a larger item is never drawn smaller but for rounding and widening; the
  view's, that no tile and no line of the rest is cut.
- `src/view.rs`: the embeddable `View`: screens (start, scanning, browse, and its folder
  scanned again after a trash or `r`: the last tree is dropped as that scan starts, what it
  showed kept by path in `Saved`, never two trees at once), the dialog, keys and mouse,
  `tick`/`next_wake` (when nothing waits, the host may block on input). `Browse::ticked` holds
  only the top ticks (`tidy`): a row in a ticked folder shows ticked, and unticking it splits
  that folder's tick (`tick_off`). A new tree clears the
  hits of the last drawing (`forget_hits`): they hold the old tree's NodeIds. Its tests drive
  it with synthetic events into a `Buffer`.
- `src/draw.rs`: the view drawn into a ratatui `Buffer` (cells only).
- `src/style.rs`: `Colors` (termcode's defaults, `fitted` to 256 or 16 colours once) and
  `IconSet` (termcode's Nerd, Unicode and ASCII glyphs, each one cell but ASCII's `[ ]` tick
  boxes: not ticked, ticked, holding a tick). Plain data; a host passes its own. Keep every
  non-colour cue (the selection's edge mark and bold, the lock, bold and crossed-out buttons,
  the tiles' class letters and the selected tile's outline, the frame of the box a lock's
  reason shows in, a row's tick box): in 16 colours the selection is as black as the pane.
  Hover is styling only:
  `View::hover_at` looks where a click looks (a lock lies on its row or tile, and a click
  there is theirs), and Move to Trash is drawn disabled exactly when a click on it
  would do nothing (`Confirm::clickable`), hovered or not; for several items, `fits` holds
  only once every path to move has been drawn whole.
- `src/main.rs`: the CLI: the terminal (raw mode, alternate screen, mouse, bracketed paste so
  pasted text never arrives as keys), put back on quit, error, panic and SIGINT/SIGTERM/SIGHUP
  (a `sigwait` thread); `xdg-open`; OSC 52; `--summary`. It fixes glibc's mmap threshold
  (`mallopt`) so a scan again peaks no higher than the first scan; the library leaves malloc
  settings to its host.
- `tests/bwrap.rs` and the guard's `rule1_and_5_real_mounts_in_a_sandbox`: real mounts made
  in a bwrap sandbox (skipped where bwrap cannot run).
- `scripts/smoke.py BINARY STEPS [--read-only] [PATH]`: the view in a pty with a temporary
  HOME and fixture (keys, waits, thread count, wake-ups, memory now and max RSS, exit code,
  terminal put back, what reached the temporary trash). It refuses a PATH outside its
  temporary folder without `--read-only`. Drive any change to the view through it and report
  what it prints.
- `scripts/bench.sh`, `examples/frame.rs`: the speed and memory benchmark (scan, view, a scan
  again, frame times, a trash).

termdisk deletes only by emptying the home trash, a separate step the user confirms
(`guard::empty_trash`, rule 13: what is in its `files` and the matching `.trashinfo`,
through folders held open, no symlink followed, no mount crossed, no permission changed).
Everything else moves one item at a time to the home trash, after the checks in
`src/guard.rs`. Keep `guard::trash` the single entry point; never add a fallback (copying,
unlinking) when the rename fails, and no other permanent delete (the user's decision). The
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
  only in trash.rs (one `unlink` there: the rollback of a `.trashinfo` made moments before,
  and `empty`), and
  `grep -nE 'remove_dir_all|remove_dir\(|set_permissions|chmod|chown|pkexec|sudo' src/*.rs`
  finds only lines marked test-only.

## Versions

Changes worth telling users go under the next version in `CHANGELOG.md`. Version `X.Y.Z`: X
for a new feature or a huge change (a new chart or view), Y for a change to an existing part
(the tree, the treemap, the scan, a CLI flag), Z for bug fixes. A release bumps only the
largest kind it holds, the lower numbers back to 0 (1.4.2 with a fix and a treemap change:
1.5.0).

## Releases

`.github/workflows/ci.yml` checks every push to main and every pull request (format, lint,
tests with the skipped ones as warnings, dependency licences, the package, rust-version), with
read-only permissions and no secrets. `.github/workflows/release.yml` runs on a `v*` tag:
those checks, the tag against `Cargo.toml` and `CHANGELOG.md`, a check that the `crates-io`
environment requires a reviewer, then `cargo publish` once the owner approves, then the GitHub
Release with that version's CHANGELOG section. Rust is pinned in `ci.yml` and in
`release.yml`'s publish job (`RUSTUP_TOOLCHAIN`, the same version: a release checks it); bump
both on purpose. Actions are pinned by commit SHA. A release refuses a `.cargo/config*` or a
`rust-toolchain*` file in the repository: they would run beside the token.

No token is stored. The publish job uses crates.io's Trusted Publishing:
`rust-lang/crates-io-auth-action` trades GitHub's OIDC token for a crates.io token that lasts
30 minutes and is revoked when the job ends. crates.io trusts only what the crate's
configuration names (crates.io, termdisk, Settings, Trusted Publishing, GitHub):

    Repository owner   JonasAtelier
    Repository name    termdisk
    Workflow filename  release.yml
    Environment        crates-io

Renaming the workflow file or the environment breaks publishing until the configuration
follows. The `crates-io` environment is what makes a person approve; it was made once with
(the owner as reviewer, only `v*` tags may use it):

    gh api -X PUT repos/JonasAtelier/termdisk/environments/crates-io --input - <<EOF
    {"reviewers": [{"type": "User", "id": $(gh api user -q .id)}],
     "deployment_branch_policy": {"protected_branches": false, "custom_branch_policies": true}}
    EOF
    gh api -X POST repos/JonasAtelier/termdisk/environments/crates-io/deployment-branch-policies \
      -f name='v*' -f type=tag

Actions, release, Run workflow on main runs the `guard` job, which says whether the
environment still needs a person; nothing is published from there.

The move from the stored token (0.2.1 was published with it), in this order: add the
configuration above on crates.io; the environment secret `CARGO_REGISTRY_TOKEN` may stay
(unused) until a release has published through Trusted Publishing; then revoke that token on
crates.io (Account Settings, API Tokens), delete the secret (`gh secret delete
CARGO_REGISTRY_TOKEN --env crates-io --repo JonasAtelier/termdisk`), and turn on "require
Trusted Publishing" in the crate's settings, so no token can publish it any more.

A release: the version in `Cargo.toml`, its `CHANGELOG.md` section dated, committed on main
and green in CI; then `git tag vX.Y.Z && git push origin vX.Y.Z`, and approve the
`crates-io` deployment in the run. If it stops after the publish (no GitHub Release), use
"Re-run failed jobs": it reruns only what failed, never a publish that succeeded (crates.io
takes a version once, so "Re-run all jobs" would fail there). An existing GitHub Release is
left as it is.
