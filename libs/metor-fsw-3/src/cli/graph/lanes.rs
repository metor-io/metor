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

/// The column of each span, 0 beside the boxes. Shorter spans go first and
/// take the innermost column free over their rows, so nested spans nest.
pub fn pack(spans: &[Span]) -> Vec<usize> {
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
