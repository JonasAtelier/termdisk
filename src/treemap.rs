// SPDX-License-Identifier: GPL-3.0-only

//! A squarified treemap (Bruls, Huizing and van Wijk, 2000) laid out in terminal cells.
//!
//! Rows of tiles are laid along the shorter side of the space left, each row taking items for
//! as long as its worst aspect ratio improves. A cell is about twice as tall as it is wide, so
//! the layout counts every row of cells as two units: the tiles then look square. Edges are
//! computed once and shared by the tiles on both sides, then rounded to cells, so the tiles
//! cover the area once, with no gap and no overlap.

use ratatui::layout::Rect;

/// What a tile stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Item {
    /// The item at this index in the sizes given.
    One(usize),
    /// This many items too small for a tile of their own, together.
    Rest(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tile {
    pub item: Item,
    pub rect: Rect,
}

/// The fewest cells an item gets a tile for (4 wide by 2 high, room for a name).
const MIN_CELLS: f64 = 8.0;

/// Tiles for `sizes` (largest first) over `area`. Items of size 0 get no tile; items that would
/// get fewer than `MIN_CELLS` cells share one `Rest` tile at the end (unless there is only one
/// of them). An item whose tile rounds to nothing is left out; the area is still covered.
pub fn layout(sizes: &[u64], area: Rect) -> Vec<Tile> {
    let total: f64 = sizes.iter().map(|&s| s as f64).sum();
    let cells = f64::from(area.width) * f64::from(area.height);
    if total == 0.0 || cells == 0.0 {
        return Vec::new();
    }
    let alone = sizes
        .iter()
        .take_while(|&&s| s as f64 / total * cells >= MIN_CELLS)
        .count();
    let mut items: Vec<(Item, f64)> = (0..alone)
        .map(|i| (Item::One(i), sizes[i] as f64))
        .collect();
    let small: Vec<usize> = (alone..sizes.len()).filter(|&i| sizes[i] > 0).collect();
    match small[..] {
        [] => {}
        [i] => items.push((Item::One(i), sizes[i] as f64)),
        _ => {
            let sum = small.iter().map(|&i| sizes[i] as f64).sum();
            items.push((Item::Rest(small.len()), sum));
        }
    }
    // In layout units: one per column, two per row of cells.
    let (w, h) = (f64::from(area.width), f64::from(area.height) * 2.0);
    let scale = w * h / items.iter().map(|it| it.1).sum::<f64>();
    let areas: Vec<f64> = items.iter().map(|it| it.1 * scale).collect();
    let mut free = Free {
        x0: 0.0,
        y0: 0.0,
        x1: w,
        y1: h,
    };
    let mut tiles = Vec::with_capacity(items.len());
    let mut i = 0;
    while i < areas.len() {
        let side = (free.x1 - free.x0).min(free.y1 - free.y0);
        let mut j = i + 1;
        while j < areas.len() && worst(&areas[i..=j], side) <= worst(&areas[i..j], side) {
            j += 1;
        }
        let last = j == areas.len();
        for (k, r) in free.take_row(&areas[i..j], last).into_iter().enumerate() {
            let rect = to_cells(r, area);
            if rect.width > 0 && rect.height > 0 {
                tiles.push(Tile {
                    item: items[i + k].0,
                    rect,
                });
            }
        }
        i = j;
    }
    tiles
}

/// The space not yet given out, in layout units.
struct Free {
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
}

impl Free {
    /// Lays `row` along the shorter side and takes its strip off; `last` takes all that is left
    /// (what the floating-point sums would miss by a hair). Returns each item's [x0, y0, x1, y1].
    fn take_row(&mut self, row: &[f64], last: bool) -> Vec<[f64; 4]> {
        let sum: f64 = row.iter().sum();
        let tall = self.x1 - self.x0 >= self.y1 - self.y0;
        // A tall space gets a column at its left; a wide one, a row at its top.
        let (along0, along1) = if tall {
            (self.y0, self.y1)
        } else {
            (self.x0, self.x1)
        };
        let (across0, across_end) = if tall {
            (self.x0, self.x1)
        } else {
            (self.y0, self.y1)
        };
        let across1 = if last {
            across_end
        } else {
            across0 + sum / (along1 - along0)
        };
        let thick = across1 - across0;
        let mut out = Vec::with_capacity(row.len());
        let mut a = along0;
        for (k, &area) in row.iter().enumerate() {
            let b = if k + 1 == row.len() {
                along1
            } else {
                a + area / thick
            };
            out.push(if tall {
                [across0, a, across1, b]
            } else {
                [a, across0, b, across1]
            });
            a = b;
        }
        if tall {
            self.x0 = across1;
        } else {
            self.y0 = across1;
        }
        out
    }
}

/// The worst (largest) aspect ratio in a row of `areas` laid along `side`.
fn worst(areas: &[f64], side: f64) -> f64 {
    let sum: f64 = areas.iter().sum();
    let max = areas.iter().copied().fold(0.0, f64::max);
    let min = areas.iter().copied().fold(f64::INFINITY, f64::min);
    let (s2, w2) = (sum * sum, side * side);
    f64::max(w2 * max / s2, s2 / (w2 * min))
}

/// A rectangle in layout units, rounded to cells of `area`.
fn to_cells([x0, y0, x1, y1]: [f64; 4], area: Rect) -> Rect {
    let (cx0, cx1) = (x0.round() as u16, x1.round() as u16);
    let (cy0, cy1) = ((y0 / 2.0).round() as u16, (y1 / 2.0).round() as u16);
    Rect::new(area.x + cx0, area.y + cy0, cx1 - cx0, cy1 - cy0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// How many tiles cover each cell of `area` (and none may lie outside it).
    fn coverage(tiles: &[Tile], area: Rect) -> Vec<u32> {
        let mut grid = vec![0u32; area.width as usize * area.height as usize];
        for t in tiles {
            let r = t.rect;
            assert!(
                r.x >= area.x && r.right() <= area.right(),
                "{r:?} outside {area:?}"
            );
            assert!(
                r.y >= area.y && r.bottom() <= area.bottom(),
                "{r:?} outside {area:?}"
            );
            for y in r.y..r.bottom() {
                for x in r.x..r.right() {
                    grid[(y - area.y) as usize * area.width as usize + (x - area.x) as usize] += 1;
                }
            }
        }
        grid
    }

    #[test]
    fn tiles_cover_the_area_once() {
        let sizes = [500, 300, 120, 80, 50, 30, 20, 10, 5, 3, 2, 1, 1];
        for area in [
            Rect::new(3, 2, 80, 24),
            Rect::new(0, 0, 120, 40),
            Rect::new(0, 0, 300, 80),
            Rect::new(5, 1, 7, 3),
        ] {
            let tiles = layout(&sizes, area);
            assert!(
                coverage(&tiles, area).iter().all(|&n| n == 1),
                "{area:?}: {tiles:?}"
            );
        }
    }

    #[test]
    fn areas_follow_sizes() {
        let sizes = [600, 300, 200, 100];
        let area = Rect::new(0, 0, 60, 20);
        let tiles = layout(&sizes, area);
        assert_eq!(tiles.len(), 4);
        for t in &tiles {
            let Item::One(i) = t.item else {
                panic!("{t:?}")
            };
            let want = sizes[i] as f64 / 1200.0 * 1200.0;
            let got = f64::from(t.rect.width) * f64::from(t.rect.height);
            // Rounding moves each edge by half a cell at most.
            let slack = f64::from(t.rect.width + t.rect.height);
            assert!((got - want).abs() <= slack, "{t:?}: {got} cells for {want}");
        }
        // Squarified: no tile is a sliver (in looks: a cell is two units tall).
        for t in &tiles {
            let (w, h) = (f64::from(t.rect.width), f64::from(t.rect.height) * 2.0);
            assert!(w.max(h) / w.min(h) < 4.0, "{t:?}");
        }
    }

    #[test]
    fn small_items_share_one_tile() {
        let mut sizes = vec![1000, 500];
        sizes.extend([1; 50]);
        let area = Rect::new(0, 0, 40, 12);
        let tiles = layout(&sizes, area);
        assert_eq!(tiles.last().unwrap().item, Item::Rest(50));
        assert!(coverage(&tiles, area).iter().all(|&n| n == 1));
    }

    #[test]
    fn degenerate_inputs() {
        let area = Rect::new(0, 0, 20, 10);
        assert!(layout(&[], area).is_empty());
        assert!(layout(&[0, 0], area).is_empty());
        assert!(layout(&[5, 3], Rect::new(0, 0, 0, 10)).is_empty());
        assert_eq!(
            layout(&[42], area),
            [Tile {
                item: Item::One(0),
                rect: area
            }]
        );
        // A zero among the sizes gets no tile.
        let tiles = layout(&[5, 0, 3], area);
        assert!(tiles.iter().all(|t| t.item != Item::One(1)) && tiles.len() == 2);
        // More items than cells: all of them in one tile.
        let tiny = Rect::new(2, 2, 3, 2);
        let tiles = layout(&[1; 100], tiny);
        assert_eq!(
            tiles,
            [Tile {
                item: Item::Rest(100),
                rect: tiny
            }]
        );
    }
}
