// SPDX-License-Identifier: GPL-3.0-only

//! How long a frame takes on a real tree: the treemap layout alone (and on 400 files of one
//! size, its hardest case) and the whole view drawn into a buffer, of the scanned folder and
//! of its widest folder shown (its first item selected: that folder's map, the rest's list),
//! at 120x40 and 300x80. Read-only; prints medians of 50.
//! `cargo run --release --example frame -- PATH` (scripts/bench.sh runs it).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use termdisk::{treemap, NodeId, Policy, Tree, View};

fn median(mut f: impl FnMut()) -> Duration {
    let mut t: Vec<Duration> = (0..50)
        .map(|_| {
            let a = Instant::now();
            f();
            a.elapsed()
        })
        .collect();
    t.sort();
    t[25]
}

/// The pointer moved to a new cell (across rows and tiles), then the whole view drawn.
fn move_and_draw(view: &mut View, buf: &mut Buffer, at: &mut u16) {
    let a = buf.area;
    *at = at.wrapping_add(7);
    view.handle_mouse(MouseEvent {
        kind: MouseEventKind::Moved,
        column: *at % a.width,
        row: 1 + *at % (a.height - 2),
        modifiers: KeyModifiers::NONE,
    });
    view.draw(a, buf);
}

/// The children's sizes of `id`, largest first, as the view lays them out.
fn sizes(tree: &Tree, id: NodeId) -> Vec<u64> {
    let mut s: Vec<u64> = tree.children(id).map(|k| tree.node(k).alloc).collect();
    s.sort_by(|a, b| b.cmp(a));
    s
}

fn main() {
    let path = PathBuf::from(std::env::args_os().nth(1).expect("usage: frame PATH"));
    let euid = unsafe { libc::geteuid() };
    let tree = termdisk::scan(&path, euid).unwrap().join().unwrap();
    let widest = (0..tree.len() as NodeId)
        .max_by_key(|&i| tree.children(i).len())
        .unwrap();
    let ready = |path: &std::path::Path| {
        let mut view = View::new(Policy::detect(true), Some(path));
        while view.next_wake().is_some() {
            view.tick();
            std::thread::sleep(Duration::from_millis(1));
        }
        view
    };
    let mut view = ready(&path);
    // The widest folder shown, its first item selected: the map is that folder's, with the
    // view's own texts and the rest's list.
    let mut wide_view = ready(&tree.path(widest));
    println!(
        "{} entries; the root has {} children, the widest folder {}",
        tree.len(),
        tree.children(0).len(),
        tree.children(widest).len()
    );
    println!("| size | layout, root | layout, widest folder | layout, 400 of one size | full draw | full draw, in the widest folder | pointer moving | by age, pointer moving |");
    println!("|---|---|---|---|---|---|---|---|");
    // 400 files of one size: as many tiles as the map has room for, so the most work for the
    // layout's table of strips (the tiles under the legend: 239x77 at 300x80).
    let one_size = vec![1000; 400];
    // Texts as narrow as the view's can be (a size of 6 cells, a share of 2, the frame, the
    // gutter): the most tiles a strip, the layout's most work. The full draw times the view's.
    let narrowest = |item: treemap::Item| match item {
        treemap::Item::One(_) => 13,
        treemap::Item::Rest(n) => format!("+{n}").len() as u16 + 3,
    };
    for (w, h) in [(120, 40), (300, 80)] {
        let map = Rect::new(61, 1, w - 61, h - 2);
        let (root, wide) = (sizes(&tree, 0), sizes(&tree, widest));
        let l_root = median(|| drop(treemap::layout(&root, map, narrowest)));
        let l_wide = median(|| drop(treemap::layout(&wide, map, narrowest)));
        let tiles = Rect::new(61, 2, w - 61, h - 3);
        let l_same = median(|| drop(treemap::layout(&one_size, tiles, narrowest)));
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        let draw = median(|| view.draw(area, &mut buf));
        let draw_wide = median(|| wide_view.draw(area, &mut buf));
        let mut at = 0;
        let hover = median(|| move_and_draw(&mut view, &mut buf, &mut at));
        let t = KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE);
        view.handle_key(t);
        let by_age = median(|| move_and_draw(&mut view, &mut buf, &mut at));
        view.handle_key(t);
        println!(
            "| {w}x{h} | {l_root:.1?} | {l_wide:.1?} | {l_same:.1?} | {draw:.1?} | {draw_wide:.1?} | {hover:.1?} | {by_age:.1?} |"
        );
    }
}
