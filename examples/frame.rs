// SPDX-License-Identifier: GPL-3.0-only

//! How long a frame takes on a real tree: the treemap layout alone and the whole view drawn
//! into a buffer, at 120x40 and 300x80. Read-only; prints medians of 50.
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
    let mut view = View::new(Policy::detect(true), Some(&path));
    while view.next_wake().is_some() {
        view.tick();
        std::thread::sleep(Duration::from_millis(1));
    }
    println!(
        "{} entries; the root has {} children, the widest folder {}",
        tree.len(),
        tree.children(0).len(),
        tree.children(widest).len()
    );
    println!("| size | layout, root | layout, widest folder | full draw | pointer moving | by age, pointer moving |");
    println!("|---|---|---|---|---|---|");
    for (w, h) in [(120, 40), (300, 80)] {
        let map = Rect::new(61, 1, w - 61, h - 2);
        let (root, wide) = (sizes(&tree, 0), sizes(&tree, widest));
        let l_root = median(|| drop(treemap::layout(&root, map)));
        let l_wide = median(|| drop(treemap::layout(&wide, map)));
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        let draw = median(|| view.draw(area, &mut buf));
        let mut at = 0;
        let hover = median(|| move_and_draw(&mut view, &mut buf, &mut at));
        let t = KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE);
        view.handle_key(t);
        let by_age = median(|| move_and_draw(&mut view, &mut buf, &mut at));
        view.handle_key(t);
        println!(
            "| {w}x{h} | {l_root:.1?} | {l_wide:.1?} | {draw:.1?} | {hover:.1?} | {by_age:.1?} |"
        );
    }
}
