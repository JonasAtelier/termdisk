# <img src="doc/media/termdisk.gif" width="56" height="56" align="absmiddle" alt=""> termdisk

![Clicking a tile in the treemap selects the matching file in the folder tree.](doc/media/overview.gif)

Find what’s taking up space on Linux. Browse folders on the left and see their largest files on the right.

## Install

1. Open a terminal on Linux. If you don’t have Rust 1.93 or newer, follow the [Rust installer](https://rust-lang.org/tools/install/) and reopen your terminal. It includes Cargo.
2. Install termdisk with Cargo.

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
