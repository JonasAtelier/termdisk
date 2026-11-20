// SPDX-License-Identifier: GPL-3.0-only

//! An ordered strip treemap (Bederson, Shneiderman and Wattenberg, 2002) laid out in terminal
//! cells.
//!
//! Strips as wide as the area go from top to bottom, each filled left to right in the order
//! given (largest first), so the tiles read like text. Areas follow the sizes compressed,
//! `size ^ COMPRESS`: every tile prints its size and its share, so its area only has to rank
//! it, and compressed, a folder whose largest item is a million times its smallest still shows
//! many of them, the bigger ones still bigger. A cell is about twice as tall as it is wide, so
//! the layout counts a column as one unit and a row as two.
//!
//! An item gets a tile of its own only where its whole text fits, never cut: the caller says
//! how many columns that takes (`layout`'s `cols`: the name on the frame's top edge, the size
//! and share under it, the frame and the gutter), and every tile is `MIN_ROWS` high at least.
//! Each strip takes its exact height in whole rows, rounded down or up, the last the rows left,
//! every one within a row of its exact height (rounding each strip's height rather than each
//! edge's place keeps strips of one height at one height). Within a strip each tile gets its
//! columns and the room left goes by area, as evenly as those allow (water-filling), the edges
//! rounded as running sums: tiles of one size and one text width differ by a column at most.
//! Where the strips break, and how each rounds, comes from a dynamic programme over the cells
//! as drawn: the most tiles of their own, then the best fit (squarest, closest to their areas).
//! What has no tile shares the last one, `Item::Rest`.

use ratatui::layout::Rect;

/// What a tile stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Item {
    /// The item at this index in the sizes given.
    One(usize),
    /// This many items without a tile of their own (too small, or their text too wide),
    /// together.
    Rest(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tile {
    pub item: Item,
    pub rect: Rect,
}

/// Areas follow `size ^ COMPRESS`: a tenth of the largest gets a quarter of its area. Looked at
/// on a home folder (7 GB to 4 kB), photos of 3 to 7 MB, ~/.cache and ~/JonasAtelier at 0.4,
/// 0.5 and 0.6: 0.5 and 0.6 gave each the same tiles, 0.6 the clearer ranking (at 0.5 the
/// second strip of ~/.cache came out taller than the first); 0.4 gave the home one more tile,
/// of 20 kB, and drew its folders of 7.1 GB and 1.3 GB nearly alike.
const COMPRESS: f64 = 0.6;
/// The lowest tile of its own, in rows: the frame's top (the name on it), the size and share,
/// the frame's bottom, the gutter.
const MIN_ROWS: u16 = 4;
/// The lowest strip the rest gets alone at the bottom: a frame of two rows, its text on the top
/// one, and the gutter.
const REST_ROWS: u16 = 3;
/// No tile in a strip with tiles of their own is drawn under this share of its area (the rest's
/// included: it may hold most of the folder). Texts widen the tiles of small items (a 399-byte
/// file needs as many columns as its name, size and share take); without a floor, six of them
/// beside a file 1 750 times larger left it no wider than they were.
const LEAST_SHARE: f64 = 0.5;

/// Tiles for `sizes` (largest first) over `area`, in that order, top to bottom and left to right.
/// `cols(item)` is the fewest columns the tile of `item` needs for its whole text, frame and
/// gutter (`Item::Rest(n)`: the tile of the `n` others, where it shares a strip; alone it takes
/// the whole width). Items of size 0 get no tile. As many items as the strips can give a tile
/// that wide and `MIN_ROWS` high get one, the largest; all others, also one alone, share one
/// `Rest` tile at the end.
pub fn layout(sizes: &[u64], area: Rect, cols: impl Fn(Item) -> u16) -> Vec<Tile> {
    if area.width == 0 || area.height == 0 || sizes.iter().all(|&s| s == 0) {
        return Vec::new();
    }
    let mut plan = Plan::new(sizes, area, &cols);
    let mut table = plan.table();
    plan.fill(&mut table, 1);
    let (mut own, mut strips) = plan.last(&table);
    let (mut most, mut tiles) = (own, plan.tiles(&strips, own, area));
    // Tiles of their own go in order, so an item no strip can hold (its text fits the width but
    // no strip has room for it beside the others) would leave every item after it without a
    // tile too. Take it out (its area joins the rest's, the others' stay as they are) and
    // choose again; the strips before it stay as they were. Only where a later item has a
    // narrower text: else the strips are full, and taking it out frees nothing. A taking out
    // that gives no more tiles does not end it: a run of such items hides all after them.
    for _ in 0..RETRIES {
        if !plan.blocks(own) {
            break;
        }
        plan.drop_tried(own);
        table.keep(own + 1);
        plan.fill(&mut table, own + 1);
        (own, strips) = plan.last(&table);
        if own > most {
            (most, tiles) = (own, plan.tiles(&strips, own, area));
        }
    }
    tiles
}

/// How many items no strip can hold `layout` takes out, at most, to give the items after them
/// tiles: four already gave most of what it gains, eight nearly all.
const RETRIES: usize = 8;

/// The DP's table (`Plan::fill`): for every count of the first items tried and every count of
/// rows, the least cost of strips holding exactly those items in those rows, and the last of
/// them (its first item, its rows); and the fewest and most rows each count can take.
struct Table {
    height: usize,
    per_strip: usize,
    best: Vec<(f64, u32, u16)>,
    span: Vec<(usize, usize)>,
}

impl Table {
    fn at(&self, k: usize, r: usize) -> usize {
        k * (self.height + 1) + r
    }

    /// Forgets all but the first `count` rows of the table (counts 0..count of items).
    fn keep(&mut self, count: usize) {
        self.best.truncate(count * (self.height + 1));
        self.span.truncate(count);
    }
}

/// One strip: tried items `first..end`, the rest after them if it holds the rest's area,
/// between the rows `top` and `bottom` of the area.
struct Strip {
    first: usize,
    end: usize,
    rest: Option<f64>,
    top: u16,
    bottom: u16,
}

/// What the layout works with, in layout units: one per column, two per row of cells.
#[derive(Clone)]
struct Plan<'a> {
    cols: &'a dyn Fn(Item) -> u16,
    width: u16,
    height: u16,
    w: f64,
    min_rows: u16,
    rest_rows: u16,
    /// The items that may have a tile of their own, in order: their index in the sizes and the
    /// columns their text takes. Those that have one are the first of them.
    tried: Vec<(usize, u16)>,
    /// Running sums of the tried items' areas: the first `k` take `sums[k]`.
    sums: Vec<f64>,
    /// The area of everything (`w` by twice the rows), and how many items are not empty.
    total: f64,
    not_empty: usize,
}

impl<'a> Plan<'a> {
    fn new(sizes: &[u64], area: Rect, cols: &'a dyn Fn(Item) -> u16) -> Plan<'a> {
        let (width, height) = (area.width, area.height);
        // Not fewer, however low the area: a tile of its own has its frame. Lower than
        // MIN_ROWS, the area holds the rest alone.
        let min_rows = MIN_ROWS;
        // The tiles of a strip share its width, so all tiles of their own together take at most
        // (strips of min_rows) x width columns: the items tried stop there. An item whose text
        // is wider than the area never has a tile; the next ones may. Each item asked about
        // takes a column at least, so a folder of 166 000 files costs a few hundred texts.
        let room = u32::from(height / min_rows) * u32::from(width);
        let (mut tried, mut weights, mut taken) = (Vec::new(), Vec::new(), 0u32);
        let (mut others, mut asked, mut not_empty) = (0.0, 0u32, 0);
        // The items asked stop once even the narrowest text met so far would overfill the
        // strips. ponytail: a later item with a narrower text still could only get a tile if
        // every strip were packed full of texts (and rows), which the layout never comes near;
        // asking on would cost a text for each of up to 4 500 items on every drawing.
        let mut narrowest = u32::from(width);
        for (i, &size) in sizes.iter().enumerate() {
            if size == 0 {
                continue;
            }
            not_empty += 1;
            if asked < room && taken + narrowest <= room {
                asked += 1;
                let c = u32::from(cols(Item::One(i)));
                if c <= u32::from(width) && taken + c <= room {
                    (taken, narrowest) = (taken + c, narrowest.min(c));
                    tried.push((i, c as u16));
                    weights.push((size as f64).powf(COMPRESS));
                    continue;
                }
            }
            others += size as f64;
        }
        // The items not tried count together, compressed as one: compressed one by one, the
        // 166 000 small files of a folder would take nearly all of it from the largest.
        let total_weight = weights.iter().sum::<f64>() + others.powf(COMPRESS);
        let total = f64::from(width) * 2.0 * f64::from(height);
        let mut sums = vec![0.0];
        for weight in weights {
            sums.push(sums[sums.len() - 1] + weight * total / total_weight);
        }
        Plan {
            cols,
            width,
            height,
            w: f64::from(width),
            min_rows,
            rest_rows: REST_ROWS.min(height),
            tried,
            sums,
            total,
            not_empty,
        }
    }

    /// An empty table for the DP: no items in no rows, at no cost.
    fn table(&self) -> Table {
        // A strip holds no more tiles than the narrowest text tried fits across it (taking
        // items out later only makes that bound looser, never wrong).
        let narrowest = self.tried.iter().map(|t| t.1).min().unwrap_or(self.width);
        let mut table = Table {
            height: usize::from(self.height),
            per_strip: usize::from(self.width / narrowest.max(1)).max(1),
            best: Vec::new(),
            span: Vec::new(),
        };
        table.best.resize(table.height + 1, (f64::INFINITY, 0, 0));
        table.best[0].0 = 0.0;
        table.span.push((0, 0));
        table
    }

    /// Fills the table for the first `from..=` items tried, those before it filled already.
    /// Every strip but the last takes its exact height in whole rows, rounded down or up
    /// (`row_choices`): a strip's cells depend on its items, its rows and its place only. It
    /// costs (items tried) x per_strip x 2 x rows.
    fn fill(&self, table: &mut Table, from: usize) {
        let (height, tried) = (table.height, self.tried.len());
        table
            .best
            .resize((tried + 1) * (height + 1), (f64::INFINITY, 0, 0));
        table.span.resize(tried + 1, (usize::MAX, 0));
        for i in from..=tried {
            for j in (i.saturating_sub(table.per_strip)..i).rev() {
                let Some(widths) = self.widths(j, i, None) else {
                    break; // too narrow for these texts, and for more of them
                };
                let (low, high) = table.span[j];
                if low > high {
                    continue; // no strips hold the first j
                }
                let [down, up] = self.row_choices(j, i);
                for rows in [down, up].into_iter().take(1 + usize::from(up != down)) {
                    let taken = usize::from(rows);
                    let cost = self.cost(j, i, None, &widths, rows);
                    let Some(cost) = cost.filter(|_| low + taken <= height) else {
                        continue;
                    };
                    for r in low..=high.min(height - taken) {
                        let cost = table.best[table.at(j, r)].0 + cost;
                        let to = table.at(i, r + taken);
                        if cost < table.best[to].0 {
                            table.best[to] = (cost, j as u32, rows);
                            let (lo, hi) = &mut table.span[i];
                            (*lo, *hi) = ((*lo).min(r + taken), (*hi).max(r + taken));
                        }
                    }
                }
            }
        }
    }

    /// The most tiles of their own the table allows, and those strips: for that many, those of
    /// the least cost. The last strip takes the rows left and the rest, or the rest has those
    /// rows alone; within a row of its exact height too where it holds tiles of their own.
    fn last(&self, table: &Table) -> (usize, Vec<Strip>) {
        let (height, tried) = (table.height, self.tried.len());
        for own in (0..=tried).rev() {
            let rest = (self.not_empty > own).then(|| self.total - self.sums[own]);
            let mut last: Option<(f64, usize, usize)> = None;
            for first in own.saturating_sub(table.per_strip)..=own {
                if first == own && rest.is_none() {
                    continue; // a strip of nothing
                }
                let Some(widths) = self.widths(first, own, rest) else {
                    continue;
                };
                // Rows the strips above may take: those that reach the first items, and with
                // tiles of their own here, leaving this strip within a row of its exact height.
                let (mut low, mut high) = table.span[first];
                if first < own {
                    let exact = self.exact_rows(first, own, rest);
                    low = low.max((height as f64 - exact - 1.0).ceil().max(0.0) as usize);
                    high = high.min((height as f64 - exact + 1.0).floor().max(0.0) as usize);
                }
                for above in low..=high.min(height - 1) {
                    let before = table.best[table.at(first, above)].0;
                    if !before.is_finite() {
                        continue; // no strips hold the first items in these rows
                    }
                    let rows = (height - above) as u16;
                    let Some(cost) = self.cost(first, own, rest, &widths, rows) else {
                        continue;
                    };
                    if last.is_none_or(|(least, ..)| before + cost < least) {
                        last = Some((before + cost, first, above));
                    }
                }
            }
            let Some((_, first, above)) = last else {
                continue;
            };
            let mut strips = vec![Strip {
                first,
                end: own,
                rest,
                top: above as u16,
                bottom: self.height,
            }];
            let (mut end, mut bottom) = (first, above);
            while end > 0 {
                let (_, first, rows) = table.best[table.at(end, bottom)];
                let (first, top) = (first as usize, bottom - usize::from(rows));
                strips.push(Strip {
                    first,
                    end,
                    rest: None,
                    top: top as u16,
                    bottom: bottom as u16,
                });
                (end, bottom) = (first, top);
            }
            strips.reverse();
            return (own, strips);
        }
        unreachable!("the rest alone over the whole area always fits")
    }

    /// The tiles of these strips, the first `own` items tried having tiles of their own.
    fn tiles(&self, strips: &[Strip], own: usize, area: Rect) -> Vec<Tile> {
        let mut tiles = Vec::with_capacity(own + 1);
        for strip in strips {
            let rows = strip.bottom - strip.top;
            // The strips chosen fit (`last` and `fill` asked `widths` too).
            let widths = self
                .widths(strip.first, strip.end, strip.rest)
                .unwrap_or_default();
            let mut left = 0;
            for (k, width) in widths.into_iter().enumerate() {
                if width > 0 && rows > 0 {
                    tiles.push(Tile {
                        item: self.item(strip.first + k, own),
                        rect: Rect::new(area.x + left, area.y + strip.top, width, rows),
                    });
                }
                left += width;
            }
        }
        tiles
    }

    /// Whether item `own` tried, the first without a tile, may be what keeps the items after it
    /// from theirs: some later one has a narrower text.
    fn blocks(&self, own: usize) -> bool {
        let Some(&(_, cols)) = self.tried.get(own) else {
            return false;
        };
        self.tried[own + 1..].iter().any(|t| t.1 < cols)
    }

    /// Takes tried item `k` out: its area joins the rest's, which is everything after the items
    /// with tiles (the running sums are what the others take, so theirs stay as they were).
    fn drop_tried(&mut self, k: usize) {
        let area = self.area(k);
        self.tried.remove(k);
        self.sums.remove(k + 1);
        for sum in &mut self.sums[k + 1..] {
            *sum -= area;
        }
    }

    /// The area of tried item `k`.
    fn area(&self, k: usize) -> f64 {
        self.sums[k + 1] - self.sums[k]
    }

    /// The exact height in rows of tried items `first..end`, and the rest's area when given,
    /// side by side across the area.
    fn exact_rows(&self, first: usize, end: usize, rest: Option<f64>) -> f64 {
        (self.sums[end] - self.sums[first] + rest.unwrap_or(0.0)) / self.w / 2.0
    }

    /// The rows a strip of tried items `first..end` may take with another strip after it: its
    /// exact height rounded down, and up; `min_rows` at least (`cost` refuses a strip drawn more
    /// than a row off its exact height). Choosing for each lets the strips share the rounding,
    /// so the last, taking the rows left, stays within a row as well (rounded alike, 35 files of
    /// one size in 57x31 were six strips of 4.43 rows drawn 4, and a last of 7).
    fn row_choices(&self, first: usize, end: usize) -> [u16; 2] {
        let exact = self.exact_rows(first, end, None);
        [exact.floor(), exact.ceil()].map(|r| (r as u16).max(self.min_rows))
    }

    /// The columns of each tile of a strip, left to right: tried items `first..end`, then the
    /// rest when it holds the rest's area. Each takes the columns of its text at least
    /// (`cols`), and the room left goes by area as evenly as that allows: a tile takes the most
    /// of its columns and its area times one level, the level that fills the width. The edges
    /// are running sums rounded once. `None` where the texts are wider than the area together.
    /// The rest alone takes the whole width.
    fn widths(&self, first: usize, end: usize, rest: Option<f64>) -> Option<Vec<u16>> {
        if first == end {
            return Some(vec![self.width]);
        }
        let rest_least = rest.map(|_| (self.cols)(Item::Rest(self.not_empty - end)));
        let count = end - first + usize::from(rest.is_some());
        // Tile k of the strip: its columns at least, and its area.
        let tile = |k: usize| match first + k < end {
            true => (self.tried[first + k].1, self.area(first + k)),
            false => (rest_least.unwrap_or(0), rest.unwrap_or(0.0)),
        };
        let least: u32 = (0..count).map(|k| u32::from(tile(k).0)).sum();
        if least > u32::from(self.width) {
            return None;
        }
        // Tiles whose share is under their columns keep their columns (marked in `out` until
        // the widths go there); the others share what is left. While Σ least <= width, the last
        // tile still sharing always stays so.
        let mut out = vec![0u16; count];
        let level = loop {
            let (mut room, mut free) = (f64::from(self.width), 0.0);
            for (k, &held) in out.iter().enumerate() {
                let (cols, area) = tile(k);
                match held > 0 {
                    true => room -= f64::from(cols),
                    false => free += area,
                }
            }
            let level = room / free;
            let mut more = false;
            for (k, held) in out.iter_mut().enumerate() {
                let (cols, area) = tile(k);
                if *held == 0 && level * area < f64::from(cols) {
                    (*held, more) = (cols.max(1), true);
                }
            }
            if !more {
                break level;
            }
        };
        let (mut extras, mut given) = (0.0, 0u16);
        for (k, width) in out.iter_mut().enumerate() {
            let (cols, area) = tile(k);
            if *width == 0 {
                extras += level * area - f64::from(cols);
            }
            let rounded = (extras.round().max(0.0) as u16).max(given);
            *width = cols + rounded - given;
            given = rounded;
        }
        // The last ends at the area's edge: what the floating-point sums would miss by a hair.
        let drawn: u32 = out.iter().map(|&w| u32::from(w)).sum();
        let last = out.len() - 1;
        let fixed = (u32::from(out[last]) + u32::from(self.width)).saturating_sub(drawn);
        out[last] = fixed as u16;
        Some(out)
    }

    /// The cost of a strip drawn `rows` high with these `widths`: for each tile, its aspect
    /// ratio (1 for a square, in looks) and how far its cells are from its area (the ratio the
    /// larger way round, so twice and half count alike). `None` if the strip holds tiles of their own and
    /// is under `min_rows`, or drawn more than a row off its exact height, or a tile in it is
    /// drawn under `LEAST_SHARE` of its area; or holds the rest alone under `REST_ROWS` rows
    /// (the rest alone may take any more).
    fn cost(
        &self,
        first: usize,
        end: usize,
        rest: Option<f64>,
        widths: &[u16],
        rows: u16,
    ) -> Option<f64> {
        let room = match end > first {
            true => {
                let exact = self.exact_rows(first, end, rest);
                rows >= self.min_rows && (f64::from(rows) - exact).abs() <= 1.0
            }
            false => rows >= self.rest_rows,
        };
        if !room {
            return None;
        }
        let areas = (first..end).map(|k| self.area(k)).chain(rest);
        let mut cost = 0.0;
        for (&cols, area) in widths.iter().zip(areas) {
            let cells = f64::from(cols) * f64::from(rows);
            let share = cells / (area / 2.0).max(1e-9); // a cell is two units
                                                        // The rest too where it shares a strip: it may hold most of the folder.
            if end > first && share < LEAST_SHARE {
                return None; // squeezed by the texts beside it
            }
            if cols > 0 {
                let looks = f64::from(cols) / (2.0 * f64::from(rows));
                cost += looks.max(1.0 / looks) + share.max(1.0 / share);
            }
        }
        Some(cost)
    }

    /// What the tile of tried item `k` stands for when the first `own` have tiles of their own.
    fn item(&self, k: usize, own: usize) -> Item {
        match k < own {
            true => Item::One(self.tried[k].0),
            false => Item::Rest(self.not_empty - own),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Texts of 11 columns for every item (frame and gutter included), and for the rest its
    /// count's: "+N" in its frame.
    fn eleven(item: Item) -> u16 {
        match item {
            Item::One(_) => 11,
            Item::Rest(n) => format!("+{n}").len() as u16 + 3,
        }
    }

    fn lay(sizes: &[u64], area: Rect) -> Vec<Tile> {
        layout(sizes, area, eleven)
    }

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
            let tiles = lay(&sizes, area);
            assert!(
                coverage(&tiles, area).iter().all(|&n| n == 1),
                "{area:?}: {tiles:?}"
            );
        }
    }

    #[test]
    fn small_items_share_one_tile() {
        let mut sizes = vec![1000, 500];
        sizes.extend([1; 50]);
        let area = Rect::new(0, 0, 40, 12);
        let tiles = lay(&sizes, area);
        let Some(Tile {
            item: Item::Rest(n),
            ..
        }) = tiles.last()
        else {
            panic!("{tiles:?}")
        };
        assert!(*n >= 40, "{tiles:?}");
        assert!(coverage(&tiles, area).iter().all(|&n| n == 1));
    }

    #[test]
    fn one_item_left_over_is_the_rest_not_a_strip_of_its_own() {
        // Its text is wider than the map: it has no tile, and is "+1" in the rest's.
        let wide = |item: Item| match item {
            Item::One(2) => 45,
            other => eleven(other),
        };
        let area = Rect::new(0, 0, 40, 12);
        let tiles = layout(&[1000, 1000, 1], area, wide);
        assert_eq!(tiles.last().unwrap().item, Item::Rest(1), "{tiles:?}");
        assert!(tiles.iter().all(|t| t.item != Item::One(2)));
        assert!(coverage(&tiles, area).iter().all(|&n| n == 1));
    }

    /// Size lists a map meets, largest first: equal files, a power law over a huge folder, a
    /// few big files among many small ones, photos of five sizes, one giant among small files,
    /// many tiny ones.
    fn mixes() -> Vec<Vec<u64>> {
        let five = [7_300_000, 6_300_000, 5_200_000, 4_200_000, 3_100_000];
        let mut photos: Vec<u64> = (0..30).map(|i| five[i % 5]).collect();
        photos.sort_by(|a, b| b.cmp(a));
        vec![
            vec![20_000; 20],
            (1..=166_000).map(|i| 1_000_000_000 / i).collect(),
            [vec![5_000_000, 3_000_000, 2_000_000], vec![4096; 300]].concat(),
            photos,
            [vec![1 << 40], vec![1000; 50]].concat(),
            vec![1; 500],
            vec![600, 300, 200, 100],
        ]
    }

    /// Maps as the view draws them (120x34, 120x40, 300x80) and smaller ones.
    const AREAS: [Rect; 8] = [
        Rect::new(61, 2, 59, 31),
        Rect::new(0, 0, 59, 36),
        Rect::new(61, 2, 239, 77),
        Rect::new(3, 1, 40, 9),
        Rect::new(0, 0, 60, 20),
        Rect::new(0, 0, 12, 5),
        Rect::new(0, 0, 20, 3),
        Rect::new(5, 1, 7, 3),
    ];

    /// The panes the sweeps over files of one size run on.
    const PANES: [Rect; 4] = [
        Rect::new(0, 0, 57, 29),
        Rect::new(0, 0, 59, 36),
        Rect::new(0, 0, 65, 29),
        Rect::new(0, 0, 120, 40),
    ];

    #[test]
    fn strips_are_shelves_read_in_order() {
        for sizes in mixes() {
            for area in AREAS {
                let tiles = lay(&sizes, area);
                assert!(coverage(&tiles, area).iter().all(|&n| n == 1), "{area:?}");
                // Two tiles share their rows, or no row at all: every strip is one shelf.
                for a in &tiles {
                    for b in &tiles {
                        let (a, b) = (a.rect, b.rect);
                        let shelf = a.y == b.y && a.height == b.height;
                        assert!(
                            shelf || a.bottom() <= b.y || b.bottom() <= a.y,
                            "{a:?} {b:?}"
                        );
                    }
                }
                // Read like text (top to bottom, left to right): the order given, the rest last.
                let mut read = tiles.clone();
                read.sort_by_key(|t| (t.rect.y, t.rect.x));
                let order: Vec<usize> = read
                    .iter()
                    .map(|t| match t.item {
                        Item::One(i) => i,
                        Item::Rest(_) => usize::MAX,
                    })
                    .collect();
                assert!(order.windows(2).all(|w| w[0] < w[1]), "{area:?}: {order:?}");
            }
        }
    }

    #[test]
    fn every_tile_has_room_for_its_whole_text() {
        for sizes in mixes() {
            for area in AREAS {
                let tiles = lay(&sizes, area);
                let rows = MIN_ROWS;
                for t in &tiles {
                    let r = t.rect;
                    let alone = tiles.iter().all(|o| o == t || o.rect.y != r.y);
                    match t.item {
                        Item::One(_) => assert!(
                            r.width >= eleven(t.item) && r.height >= rows,
                            "{area:?}: {t:?}"
                        ),
                        // The rest: its "+N" where it shares a strip; alone, the whole width
                        // and its frame of two rows and the gutter.
                        Item::Rest(_) => {
                            assert!(alone || r.width >= eleven(t.item), "{area:?}: {t:?}");
                            assert!(r.height >= REST_ROWS.min(area.height), "{area:?}: {t:?}");
                        }
                    }
                }
            }
        }
    }

    /// Whether a strip of these tiles holds one at its text's width: the room is not shared by
    /// area there, so the others may be narrower than their areas say (label-width widening).
    fn widened(strip: &[&Tile], cols: &impl Fn(Item) -> u16) -> bool {
        strip.iter().any(|t| t.rect.width <= cols(t.item))
    }

    /// No inversion where no text widens a tile: a tile's cells are within e = width + rows + 1
    /// of its compressed area when its strip shares the room by area (its width off by under a
    /// column, its strip by a row at most, and W'R - w·h <= W' + h <= W' + R + 1), so for items
    /// i before j, the larger and the smaller, cells(i) + e(i) + e(j) >= cells(j). It proves
    /// nothing where a strip holds a tile at its text's width: there that text decides, and a
    /// small item with a long name may be drawn larger than a larger neighbour (down to half
    /// its area at most, `no_tile_is_drawn_under_half_its_area`; the share on each tile is
    /// right).
    #[test]
    fn a_larger_item_is_never_drawn_smaller() {
        let check = |sizes: &[u64], area: Rect| {
            let tiles = lay(sizes, area);
            let strip = |t: &Tile| -> Vec<&Tile> {
                tiles.iter().filter(|o| o.rect.y == t.rect.y).collect()
            };
            let fair: Vec<&Tile> = tiles
                .iter()
                .filter(|t| matches!(t.item, Item::One(_)) && !widened(&strip(t), &eleven))
                .collect();
            let cells = |t: &Tile| u32::from(t.rect.width) * u32::from(t.rect.height);
            let slack = |t: &Tile| u32::from(t.rect.width + t.rect.height) + 1;
            for (k, a) in fair.iter().enumerate() {
                for b in &fair[k + 1..] {
                    assert!(
                        cells(a) + slack(a) + slack(b) >= cells(b),
                        "{area:?}: {a:?} before {b:?}"
                    );
                }
            }
        };
        for sizes in mixes() {
            for area in AREAS {
                check(&sizes, area);
            }
        }
        for area in PANES {
            for n in 1..=80 {
                check(&vec![1000; n], area);
            }
        }
    }

    #[test]
    fn equal_sizes_get_widths_a_column_apart_at_most() {
        for (area, most) in [
            (Rect::new(0, 0, 59, 36), 60),
            (Rect::new(61, 2, 57, 29), 60),
            (Rect::new(0, 0, 65, 29), 60),
            (Rect::new(61, 2, 239, 77), 400),
        ] {
            for n in 1..=most {
                let tiles = lay(&vec![1000; n], area);
                let own: Vec<&Tile> = tiles
                    .iter()
                    .filter(|t| matches!(t.item, Item::One(_)))
                    .collect();
                for t in &own {
                    let shelf = own.iter().filter(|o| o.rect.y == t.rect.y);
                    let (lo, hi) = shelf.fold((u16::MAX, 0), |(lo, hi), o| {
                        (lo.min(o.rect.width), hi.max(o.rect.width))
                    });
                    assert!(
                        hi - lo <= 1,
                        "{n} in {area:?}: widths {lo} to {hi} in a strip"
                    );
                }
            }
        }
    }

    /// How many items `layout` gives a tile of their own.
    fn own_count(sizes: &[u64], area: Rect) -> usize {
        own_with(sizes, area, eleven)
    }

    fn own_with(sizes: &[u64], area: Rect, cols: impl Fn(Item) -> u16) -> usize {
        let tiles = layout(sizes, area, cols);
        tiles
            .iter()
            .filter(|t| matches!(t.item, Item::One(_)))
            .count()
    }

    /// How many tiles of their own the strips allow the items tried, in order, before any is
    /// taken out (`Plan::fill` and `Plan::last` alone).
    fn first_choice(plan: &Plan) -> usize {
        let mut table = plan.table();
        plan.fill(&mut table, 1);
        plan.last(&table).0
    }

    #[test]
    fn strips_of_one_size_are_drawn_a_row_apart_at_most() {
        // Six strips of 4.43 rows each and a last one: rounded alike they were 4 rows and the
        // last 7.
        let tiles = lay(&[1000; 35], Rect::new(0, 0, 57, 31));
        let heights = tiles.iter().map(|t| t.rect.height);
        let (lo, hi) = (heights.clone().min().unwrap(), heights.max().unwrap());
        assert!(hi - lo <= 1, "heights {lo} to {hi}: {tiles:?}");
    }

    /// One more file of the same size costs at most a strip of tiles, `width / 11` with texts
    /// of 11 columns: keep the strips of one file less (equal sizes weigh alike, compressed or
    /// not; each exact height shrinks as the total grows, so the rounding chosen does not grow,
    /// and the widths of equal items with equal texts depend on their count only) and fold the
    /// last strip's tiles into the rest, which keeps that strip's rows, MIN_ROWS or more. That
    /// holds while the strips stay MIN_ROWS - 1 rows high exactly; once a full strip of these
    /// files is lower, no tile has room and the count falls to nothing. Not one at most: where
    /// n files fill the area exactly, the rest of n + 1 needs a strip's room.
    #[test]
    fn one_more_file_of_one_size_costs_at_most_a_strip() {
        let mut costly = Vec::new();
        for area in PANES {
            let per_strip = usize::from(area.width / 11);
            let mut last = 0;
            for n in 1..=80 {
                let sizes = vec![1000; n];
                let plan = Plan::new(&sizes, area, &eleven);
                let own = own_count(&sizes, area);
                let full = per_strip.min(plan.tried.len());
                let tall_enough = plan.exact_rows(0, full, None) > f64::from(MIN_ROWS - 1) + 1e-9;
                if tall_enough && own + per_strip < last {
                    costly.push((area.width, area.height, n, last, own));
                }
                last = own;
            }
        }
        assert!(
            costly.is_empty(),
            "(width, height, files, tiles with one less, tiles): {costly:?}"
        );
        // Where it fits, nothing is lost at all.
        let area = Rect::new(0, 0, 57, 29);
        assert_eq!(
            (own_count(&[1000; 20], area), own_count(&[1000; 21], area)),
            (20, 21)
        );
    }

    /// The most tiles of their own any cut of the items the plan tries into strips allows,
    /// found by trying them all: every set of strip breaks, both row choices of each strip but
    /// the last, which takes the rows left, the rest in the last strip or alone below it. The
    /// oracle for the DP (`first_choice`). (The plan tries every item whose text fits the width,
    /// while their texts together fit the strips: tiles of their own share a strip's width, so
    /// no layout has more.)
    fn most_by_trying_every_cut(plan: &Plan) -> usize {
        for own in (1..=plan.tried.len()).rev() {
            let rest = (plan.not_empty > own).then(|| plan.total - plan.sums[own]);
            if fits_from(plan, 0, own, rest, 0) {
                return own;
            }
        }
        0
    }

    /// The most tiles of their own any choice of the items tried allows, not only the first of
    /// them in order: every subset, the others in the rest, every cut of it. `layout` takes
    /// out an item no strip holds and tries again (`blocks`, at most `RETRIES` times), so it
    /// lies between `most_by_trying_every_cut` and this.
    fn most_by_any_choice(plan: &Plan) -> usize {
        let n = plan.tried.len();
        let mut most = 0;
        for keep in 0u32..1 << n {
            let count = keep.count_ones() as usize;
            if count <= most {
                continue;
            }
            let mut some = plan.clone();
            some.tried.clear();
            some.sums = vec![0.0];
            for k in (0..n).filter(|k| keep >> k & 1 == 1) {
                some.tried.push(plan.tried[k]);
                some.sums
                    .push(some.sums[some.sums.len() - 1] + plan.area(k));
            }
            let rest = (plan.not_empty > count).then(|| plan.total - some.sums[count]);
            if fits_from(&some, 0, count, rest, 0) {
                most = count;
            }
        }
        most
    }

    /// Whether tried items `first..own`, and the rest, fit in the rows from `used` down.
    fn fits_from(plan: &Plan, first: usize, own: usize, rest: Option<f64>, used: u16) -> bool {
        let fits = |first, end, rest, rows| {
            let widths = plan.widths(first, end, rest);
            widths.is_some_and(|w| plan.cost(first, end, rest, &w, rows).is_some())
        };
        let last_holds_something = first < own || rest.is_some();
        if last_holds_something && used < plan.height && fits(first, own, rest, plan.height - used)
        {
            return true;
        }
        (first + 1..=own).any(|end| {
            plan.row_choices(first, end).into_iter().any(|rows| {
                used + rows <= plan.height
                    && fits(first, end, None, rows)
                    && fits_from(plan, end, own, rest, used + rows)
            })
        })
    }

    #[test]
    fn as_many_items_get_a_tile_as_any_strips_allow() {
        // Small lists, so every cut can be tried: equal sizes, and sizes spread at random (a
        // fixed sequence), largest first; texts of 11 columns, and of 11 to 50 at random (the
        // wide ones leave no strip beside them, and the items after them want theirs).
        let mut seed: u64 = 1;
        let mut next = move || {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            seed >> 33
        };
        let mut lists: Vec<Vec<u64>> = (1..=10).map(|n| vec![1000; n]).collect();
        for _ in 0..60 {
            let n = 1 + next() as usize % 10;
            let mut sizes: Vec<u64> = (0..n).map(|_| 1 + next() % (1 << (next() % 20))).collect();
            sizes.sort_by(|a, b| b.cmp(a));
            lists.push(sizes);
        }
        let texts: Vec<u16> = (0..10).map(|_| 11 + (next() % 40) as u16).collect();
        let varied = |item: Item| match item {
            Item::One(i) => texts[i],
            rest => eleven(rest),
        };
        for area in [
            Rect::new(0, 0, 40, 12),
            Rect::new(0, 0, 57, 9),
            Rect::new(0, 0, 30, 16),
            Rect::new(0, 0, 24, 8),
            Rect::new(61, 2, 59, 31),
            Rect::new(0, 0, 20, 6),
        ] {
            for sizes in &lists {
                for (plan, own) in [
                    (
                        Plan::new(sizes, area, &eleven),
                        own_with(sizes, area, eleven),
                    ),
                    (
                        Plan::new(sizes, area, &varied),
                        own_with(sizes, area, varied),
                    ),
                ] {
                    let first = first_choice(&plan);
                    assert_eq!(
                        first,
                        most_by_trying_every_cut(&plan),
                        "{sizes:?} in {area:?}"
                    );
                    assert!(own >= first, "{sizes:?} in {area:?}: {own} after {first}");
                    if plan.tried.len() <= 8 {
                        let most = most_by_any_choice(&plan);
                        assert!(own <= most, "{sizes:?} in {area:?}: {own} of {most}");
                    }
                }
            }
        }
    }

    /// The home folder of `every_folder_with_room_for_a_tile_gets_one` with a folder of 1.5 GB
    /// fourth, its name 51 characters long: a text of `long` columns.
    fn home_with_a_long_name(long: u16) -> (Vec<u64>, impl Fn(Item) -> u16) {
        let home = vec![
            7_100_000, 5_700_000, 4_200_000, 1_500_000, 1_300_000, 1_100_000, 928_000, 259_000,
            117_500, 20, 8, 4, 4,
        ];
        let cols = move |item: Item| match item {
            Item::One(3) => long,
            Item::One(_) => 16,
            rest => eleven(rest),
        };
        (home, cols)
    }

    #[test]
    fn an_item_no_strip_holds_leaves_the_others_their_tiles() {
        let area = Rect::new(61, 2, 57, 29);
        // 56 columns: as wide as the map nearly, too wide to share a strip, too small for one
        // of its own. The items after it keep their tiles; it is in the rest.
        let (home, cols) = home_with_a_long_name(56);
        let tiles = layout(&home, area, cols);
        let own = tiles
            .iter()
            .filter(|t| matches!(t.item, Item::One(_)))
            .count();
        assert!(own >= 8, "{tiles:?}");
        assert!(tiles.iter().all(|t| t.item != Item::One(3)), "{tiles:?}");
        assert!(coverage(&tiles, area).iter().all(|&n| n == 1));
        // No fewer than where its text is wider than the map, and so never tried.
        let (home, wider) = home_with_a_long_name(58);
        let (home2, fits) = home_with_a_long_name(56);
        assert!(own_with(&home2, area, fits) >= own_with(&home, area, wider));
    }

    #[test]
    fn a_run_of_items_no_strip_holds_leaves_the_others_their_tiles() {
        // 32 files of one size, texts of 14 columns but four in a row of 15 to 17 (items 4 to
        // 7). Without taking them out, 5 tiles; stopping after two takings out that gave
        // nothing, 5 too.
        let area = Rect::new(0, 0, 57, 29);
        let cols = |item: Item| match item {
            Item::One(4) => 15,
            Item::One(5) => 16,
            Item::One(6) => 17,
            Item::One(7) => 16,
            Item::One(_) => 14,
            rest => eleven(rest),
        };
        let own = own_with(&vec![1000; 32], area, cols);
        assert!(own >= 20, "{own} tiles");
    }

    #[test]
    fn no_tile_is_drawn_under_half_its_area() {
        // Texts of many widths, so some tiles widen and their neighbours give way.
        let varied = |item: Item| match item {
            Item::One(i) => 11 + (i * 7 % 23) as u16,
            rest => eleven(rest),
        };
        for sizes in mixes() {
            for area in AREAS {
                let plan = Plan::new(&sizes, area, &varied);
                let share = |i: usize| {
                    plan.tried
                        .iter()
                        .position(|t| t.0 == i)
                        .map(|k| plan.area(k))
                };
                let tiles = layout(&sizes, area, varied);
                let own_area: f64 = tiles
                    .iter()
                    .filter_map(|t| match t.item {
                        Item::One(i) => share(i),
                        Item::Rest(_) => None,
                    })
                    .sum();
                for t in &tiles {
                    let shares_a_strip = tiles.iter().any(|o| o != t && o.rect.y == t.rect.y);
                    let area_of = match t.item {
                        Item::One(i) => share(i).unwrap(),
                        // Where the rest shares a strip, it is held to it too.
                        Item::Rest(_) if shares_a_strip => plan.total - own_area,
                        Item::Rest(_) => continue,
                    };
                    let cells = f64::from(t.rect.width) * f64::from(t.rect.height);
                    assert!(
                        cells >= LEAST_SHARE * area_of / 2.0 - 1e-6,
                        "{area:?} {t:?}: {cells} cells of {:.1}",
                        area_of / 2.0
                    );
                }
            }
        }
    }

    #[test]
    fn every_folder_with_room_for_a_tile_gets_one() {
        // A home folder (sizes in kB, largest first) in a 57x29 map: Downloads, Videos, .local,
        // Projects, .cache and Music each have room for their texts.
        let home = [
            7_100_000, 5_700_000, 4_200_000, 1_300_000, 1_100_000, 928_000, 259_000, 117_500, 20,
            8, 4, 4,
        ];
        let tiles = lay(&home, Rect::new(61, 2, 57, 29));
        for i in 0..=5 {
            assert!(
                tiles.iter().any(|t| t.item == Item::One(i)),
                "{i}: {tiles:?}"
            );
        }
        // One more file of the same size: here nothing lost; where the strips are full, the
        // rest needs rows of its own (see `one_more_file_of_one_size_costs_at_most_a_strip`).
        let area = Rect::new(61, 2, 57, 29);
        assert_eq!(own_count(&[1000; 20], area), 20);
        assert_eq!(own_count(&[1000; 21], area), 21);
    }

    #[test]
    fn degenerate_inputs() {
        let area = Rect::new(0, 0, 20, 10);
        assert!(lay(&[], area).is_empty());
        assert!(lay(&[0, 0], area).is_empty());
        assert!(lay(&[5, 3], Rect::new(0, 0, 0, 10)).is_empty());
        assert_eq!(
            lay(&[42], area),
            [Tile {
                item: Item::One(0),
                rect: area
            }]
        );
        // A zero among the sizes gets no tile.
        let tiles = lay(&[5, 0, 3], area);
        assert!(tiles.iter().all(|t| t.item != Item::One(1)) && tiles.len() == 2);
        // Texts wider than the area: all of them in one tile.
        let tiny = Rect::new(2, 2, 3, 2);
        let tiles = lay(&[1; 100], tiny);
        assert_eq!(
            tiles,
            [Tile {
                item: Item::Rest(100),
                rect: tiny
            }]
        );
    }
}
