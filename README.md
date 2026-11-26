<h1 align="right"><img src="doc/media/termdisk-drive-panel.gif" width="96%" alt="termdisk"></h1>

<p align="center">
  <a href="https://crates.io/crates/termdisk"><img src="https://img.shields.io/crates/v/termdisk" alt="crates.io version"></a>
  <a href="https://github.com/JonasAtelier/termdisk/actions/workflows/ci.yml?query=branch%3Amain"><img src="https://github.com/JonasAtelier/termdisk/actions/workflows/ci.yml/badge.svg?branch=main" alt="CI status on main"></a>
  <a href="LICENSE"><img src="https://img.shields.io/crates/l/termdisk" alt="License: GPL-3.0-only"></a>
</p>

![Clicking a tile in the treemap selects the matching file in the folder tree.](doc/media/overview.gif)

Find what’s taking up space on Linux. Browse folders on the left and see their largest files on the right.

## Install

1. Open a terminal on Linux.
2. Install termdisk in one of two ways.
   - **Prebuilt binary** for x86_64 or aarch64, with glibc 2.34 or newer (Ubuntu 22.04, Debian 12, Fedora 35, RHEL 9 or later). Download it, check it against the release’s checksums, and install it:

     ```sh
     t=termdisk-$(uname -m)-unknown-linux-gnu
     curl -fLO https://github.com/JonasAtelier/termdisk/releases/latest/download/$t.tar.gz
     curl -fLO https://github.com/JonasAtelier/termdisk/releases/latest/download/SHA256SUMS
     sha256sum -c --ignore-missing SHA256SUMS &&
       tar -xzf $t.tar.gz && sudo install -m 755 $t/termdisk /usr/local/bin/
     ```

     With [cargo-binstall](https://github.com/cargo-bins/cargo-binstall), `cargo binstall termdisk` does the same.
   - **With Cargo.** If you don’t have Rust 1.93 or newer, follow the [Rust installer](https://rust-lang.org/tools/install/) and reopen your terminal. It includes Cargo.

     ```sh
     cargo install termdisk --locked
     ```

3. Try a read-only scan of your Downloads folder.

   ```sh
   termdisk --read-only ~/Downloads
   ```

Press `q` to quit or `?` for shortcuts. To choose a folder and enable moving files to Trash, run `termdisk` on its own.

## Speed & memory

![Warm-cache scan of 2.05 million entries on a 12-thread machine. termdisk summary took 0.47 s, ncdu export 2.50 s, and du 3.31 s. In a separate scan of 1.24 million entries, the interactive view peaked at 88 MB including a rescan.](doc/media/performance.svg)

[Benchmark script](scripts/bench.sh)

## See it in action

**Explore by click.** Double-click a folder in the tree to expand it. Click a file inside and the treemap switches to that folder, highlighting the file.

![Clicking through Film and Audio changes the right pane from video files to audio tracks.](doc/media/navigate.gif)

**Drag to clean up.** Drag across the checkboxes, press `Del`, then review the paths and click **Move to Trash**. You can restore the files from your file manager’s Trash.

![Three previews are drag-selected and moved to Trash after confirmation. The final render stays in the folder.](doc/media/drag-to-trash.gif)

## License

© 2026 JonasAtelier · [GPL-3.0-only](LICENSE) · [Third-party notices](THIRD-PARTY-LICENSES)
