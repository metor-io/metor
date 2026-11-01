//! Edges drawn as vertical lanes beside a column of boxes, as
//! `git log --graph` draws branches.

use crate::cli::ui::theme::Lines;

/// One output's edges: the row it leaves from and the rows it enters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub source: usize,
    pub sinks: Vec<usize>,
}

impl Span {
    fn rows(&self) -> impl Iterator<Item = usize> + '_ {
        core::iter::once(self.source).chain(self.sinks.iter().copied())
    }

    fn top(&self) -> usize {
        self.rows().min().unwrap_or(self.source)
    }

    fn bottom(&self) -> usize {
        self.rows().max().unwrap_or(self.source)
    }

    fn overlaps(&self, other: &Span) -> bool {
        self.top() <= other.bottom() && other.top() <= self.bottom()
    }
}

/// The most spans [`order`] orders exactly; past it, greedily.
const EXACT: usize = 12;

/// The column of each span, 0 beside the boxes: the layout that draws the
/// fewest crossings, then the narrowest, of [`nest`] and [`stack`] over an
/// [`order`] refined by swapping neighbours.
pub fn pack(spans: &[Span]) -> Vec<usize> {
    let stacked = stack(spans, &refine(spans, order(spans)));
    let nested = nest(spans);
    if drawn(spans, &nested) < drawn(spans, &stacked) {
        nested
    } else {
        stacked
    }
}

/// Shorter spans first, each in the innermost column free over its rows.
fn nest(spans: &[Span]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..spans.len()).collect();
    order.sort_by_key(|&i| (spans[i].bottom() - spans[i].top(), spans[i].top()));
    let mut columns: Vec<Vec<usize>> = Vec::new();
    let mut placed = vec![0; spans.len()];
    for i in order {
        let free = columns
            .iter()
            .position(|taken| taken.iter().all(|&j| !spans[i].overlaps(&spans[j])));
        let column = free.unwrap_or_else(|| {
            columns.push(Vec::new());
            columns.len() - 1
        });
        columns[column].push(i);
        placed[i] = column;
    }
    placed
}

/// Spans in `order` from the boxes out, each one column past the outermost
/// span it overlaps, so spans that don't overlap share columns.
fn stack(spans: &[Span], order: &[usize]) -> Vec<usize> {
    let mut columns: Vec<Option<usize>> = vec![None; spans.len()];
    for &i in order {
        let inner = (0..spans.len())
            .filter(|&j| spans[i].overlaps(&spans[j]))
            .filter_map(|j| columns[j])
            .max();
        columns[i] = Some(inner.map_or(0, |c| c + 1));
    }
    columns.into_iter().map(|c| c.unwrap_or(0)).collect()
}

/// The crossings `columns` draws, then its width.
fn drawn(spans: &[Span], columns: &[usize]) -> (usize, usize) {
    let mut total = 0;
    for (i, outer) in spans.iter().enumerate() {
        for (j, inner) in spans.iter().enumerate() {
            if columns[j] < columns[i] {
                total += crossings(outer, inner);
            }
        }
    }
    (total, width(columns))
}

/// `order` with neighbours swapped while a swap draws fewer crossings.
fn refine(spans: &[Span], mut order: Vec<usize>) -> Vec<usize> {
    let mut best = drawn(spans, &stack(spans, &order));
    for _ in 0..order.len() {
        let mut better = false;
        for k in 1..order.len() {
            order.swap(k - 1, k);
            let score = drawn(spans, &stack(spans, &order));
            if score < best {
                (best, better) = (score, true);
            } else {
                order.swap(k - 1, k);
            }
        }
        if !better {
            break;
        }
    }
    order
}

/// The rows of `outer` that cross `inner`'s line when `outer` lies outside.
fn crossings(outer: &Span, inner: &Span) -> usize {
    let within = |row: &usize| inner.top() < *row && *row < inner.bottom();
    outer.rows().filter(within).count()
}

/// What placing `next` outside every span in `inside` costs: its crossings,
/// then how many shorter spans it holds inside it, so short spans nest.
fn cost(spans: &[Span], next: usize, inside: impl Iterator<Item = usize>) -> (usize, usize) {
    let length = |s: &Span| s.bottom() - s.top();
    inside.fold((0, 0), |(cross, nest), j| {
        let longer = length(&spans[j]) > length(&spans[next]);
        (
            cross + crossings(&spans[next], &spans[j]),
            nest + usize::from(longer),
        )
    })
}

/// The spans from innermost out, ordered to cross the fewest lines.
fn order(spans: &[Span]) -> Vec<usize> {
    match spans.len() {
        0..=EXACT => exact(spans),
        _ => greedy(spans),
    }
}

/// The cheapest order, by a search over which spans lie inside.
fn exact(spans: &[Span]) -> Vec<usize> {
    let n = spans.len();
    let members = |set: usize| (0..n).filter(move |j| set & (1 << j) != 0);
    let mut best: Vec<Option<((usize, usize), usize)>> = vec![None; 1 << n];
    best[0] = Some(((0, 0), 0));
    for set in 0..(1usize << n) {
        let Some(((cross, nest), _)) = best[set] else {
            continue;
        };
        for next in (0..n).filter(|j| set & (1 << j) == 0) {
            let (c, s) = cost(spans, next, members(set));
            let total = ((cross + c, nest + s), next);
            let slot = &mut best[set | (1 << next)];
            if slot.is_none_or(|(old, _)| total.0 < old) {
                *slot = Some(total);
            }
        }
    }
    let mut order = Vec::with_capacity(n);
    let mut set = (1usize << n) - 1;
    while let Some((_, last)) = best[set].filter(|_| set != 0) {
        order.push(last);
        set &= !(1 << last);
    }
    order.reverse();
    order
}

/// Each step, the span cheapest to place outside those placed.
fn greedy(spans: &[Span]) -> Vec<usize> {
    let mut order: Vec<usize> = Vec::with_capacity(spans.len());
    while order.len() < spans.len() {
        let next = (0..spans.len())
            .filter(|i| !order.contains(i))
            .min_by_key(|&i| (cost(spans, i, order.iter().copied()), spans[i].top()));
        order.extend(next);
    }
    order
}

/// How many characters wide the lanes for `columns` are, arrows included.
pub fn width(columns: &[usize]) -> usize {
    columns.iter().max().map_or(0, |max| 2 * (max + 1) + 1)
}

/// The lanes for `height` rows, one line each, every line [`width`] wide.
pub fn draw(height: usize, spans: &[Span], columns: &[usize], lines: &Lines) -> Vec<Vec<char>> {
    let width = width(columns);
    let mut grid = vec![vec![' '; width]; height];
    let x = |column: usize| width - 1 - 2 * (column + 1);
    for (span, &column) in spans.iter().zip(columns) {
        for row in grid.iter_mut().take(span.bottom() + 1).skip(span.top()) {
            row[x(column)] = lines.vertical;
        }
    }
    for (span, &column) in spans.iter().zip(columns) {
        for row in span.rows() {
            if let Some(line) = grid.get_mut(row) {
                line[x(column)] = corner(span, row, lines);
            }
        }
    }
    for (span, &column) in spans.iter().zip(columns) {
        let sinks = core::iter::once(false).chain(core::iter::repeat(true));
        for (row, sink) in span.rows().zip(sinks) {
            let Some(line) = grid.get_mut(row) else {
                continue;
            };
            for cell in &mut line[x(column) + 1..width - 1] {
                *cell = join(*cell, lines);
            }
            line[width - 1] = if sink { lines.arrow } else { lines.horizontal };
        }
    }
    grid
}

/// `cell` with a horizontal line drawn through it. Two edges into one input
/// meet where the inner lane turns.
fn join(cell: char, lines: &Lines) -> char {
    match cell {
        c if c == lines.vertical || c == lines.tee_right => lines.cross,
        c if c == lines.up_right => lines.tee_up,
        c if c == lines.down_right => lines.tee_down,
        _ => lines.horizontal,
    }
}

/// The lane's character where it turns into `row`.
fn corner(span: &Span, row: usize, lines: &Lines) -> char {
    match (row > span.top(), row < span.bottom()) {
        (true, true) => lines.tee_right,
        (false, true) => lines.down_right,
        (true, false) => lines.up_right,
        (false, false) => lines.horizontal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::ui::theme::{ASCII, UNICODE};

    fn span(source: usize, sinks: &[usize]) -> Span {
        Span {
            source,
            sinks: sinks.to_vec(),
        }
    }

    fn picture(height: usize, spans: &[Span]) -> String {
        let columns = pack(spans);
        let grid = draw(height, spans, &columns, &UNICODE.lines);
        let lines: Vec<String> = grid
            .iter()
            .map(|line| line.iter().collect::<String>().trim_end().to_string())
            .collect();
        lines.join("\n")
    }

    #[test]
    fn test_pack_nests_and_reuses_columns() {
        let spans = [span(0, &[6]), span(1, &[2]), span(3, &[5])];
        assert_eq!(
            pack(&spans),
            [1, 0, 0],
            "disjoint short spans share column 0"
        );
        assert_eq!(
            pack(&[span(0, &[3]), span(3, &[5])]),
            [1, 0],
            "a shared row is a clash"
        );
        assert!(pack(&[]).is_empty());
    }

    #[test]
    fn test_pack_puts_the_lane_that_taps_most_inside() {
        let spans = [span(0, &[3, 5, 7, 9]), span(1, &[9])];
        assert_eq!(drawn(&spans, &nest(&spans)).0, 3, "shortest inside");
        assert_eq!(pack(&spans), [0, 1]);
        assert_eq!(drawn(&spans, &pack(&spans)).0, 1);
    }

    #[test]
    fn test_pack_never_draws_more_than_nest() {
        let mut seed = 0x2545_f491_u64;
        let mut next = |bound: u64| {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            ((seed >> 33) % bound) as usize
        };
        for case in 0..60 {
            let count = 2 + case % 16;
            let spans: Vec<Span> = (0..count)
                .map(|_| {
                    let sinks: Vec<usize> = (0..1 + next(3)).map(|_| next(40) + 1).collect();
                    span(next(40), &sinks)
                })
                .collect();
            let packed = drawn(&spans, &pack(&spans));
            assert!(packed <= drawn(&spans, &nest(&spans)), "case {case}");
            let columns = pack(&spans);
            for (i, a) in spans.iter().enumerate() {
                for (j, b) in spans.iter().enumerate() {
                    assert!(
                        i == j || !a.overlaps(b) || columns[i] != columns[j],
                        "case {case}"
                    );
                }
            }
        }
        assert!(pack(&[]).is_empty());
    }

    #[test]
    fn test_draw_forward_fan_out_and_crossing() {
        let spans = [span(0, &[2, 4]), span(1, &[3])];
        assert_eq!(picture(5, &spans), "╭────\n│ ╭──\n├─┼─▶\n│ ╰─▶\n╰───▶");
    }

    #[test]
    fn test_draw_fan_in_joins_in_either_order() {
        let joined = "╭────\n│ ╭──\n╰─┴─▶";
        assert_eq!(picture(3, &[span(0, &[2]), span(1, &[2])]), joined);
        assert_eq!(picture(3, &[span(1, &[2]), span(0, &[2])]), joined);
        assert_eq!(
            picture(3, &[span(2, &[0]), span(1, &[0])]),
            "╭─┬─▶\n│ ╰──\n╰────",
            "loops meet from below"
        );
    }

    #[test]
    fn test_draw_loop_runs_up() {
        assert_eq!(picture(3, &[span(2, &[0])]), "╭─▶\n│\n╰──");
    }

    #[test]
    fn test_draw_ascii_and_nothing() {
        let spans = [span(0, &[1])];
        let grid = draw(2, &spans, &pack(&spans), &ASCII.lines);
        assert_eq!(grid, [vec!['+', '-', '-'], vec!['+', '-', '>']]);
        assert_eq!(width(&[]), 0);
        assert!(draw(2, &[], &[], &UNICODE.lines).iter().all(Vec::is_empty));
    }
}
