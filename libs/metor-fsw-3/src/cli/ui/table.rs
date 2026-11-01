//! A borderless table: a muted header row, then aligned rows.

use anstyle::Style;
use std::io::{self, Write};

use super::glyph;
use super::theme::Theme;
use super::{pad, width};

/// How a column aligns its cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

/// Columns of text, measured and written in one pass.
#[derive(Debug)]
pub struct Table<'a> {
    headers: &'a [(&'a str, Align)],
    rows: Vec<Vec<String>>,
    /// The widest a line may be. The last column is cut to fit.
    width: Option<usize>,
}

/// The gap between columns.
const GAP: &str = "  ";

impl<'a> Table<'a> {
    pub fn new(headers: &'a [(&'a str, Align)]) -> Self {
        Self {
            headers,
            rows: Vec::new(),
            width: None,
        }
    }

    /// Cuts the last column so each line, with its indent, fits `width`.
    pub fn fit(mut self, width: Option<usize>) -> Self {
        self.width = width;
        self
    }

    /// Adds a row. Missing cells are blank.
    pub fn row(&mut self, cells: Vec<String>) {
        self.rows.push(cells);
    }

    /// Writes the table, indented by `indent`, with the first column bold.
    pub fn write(&self, out: &mut impl Write, theme: &Theme, indent: &str) -> io::Result<()> {
        let widths = self.widths();
        let muted = theme.muted;
        let headers: Vec<&str> = self.headers.iter().map(|(header, _)| *header).collect();
        let header = self.line(&headers, &widths, 0);
        writeln!(out, "{indent}{muted}{header}{muted:#}")?;
        let bold = theme.bold(Style::new());
        for row in &self.rows {
            let cells: Vec<&str> = row.iter().map(String::as_str).collect();
            let (first, rest) = cells.split_first().unwrap_or((&"", &[]));
            let rest = self.line(rest, &widths, 1);
            let room = self
                .width
                .map(|room| room.saturating_sub(width(indent) + widths[0] + GAP.len()));
            let rest = cut(&rest, room, glyph::ELLIPSIS);
            let padding = if rest.is_empty() {
                String::new()
            } else {
                pad(first, widths[0])
            };
            write!(out, "{indent}{bold}{first}{bold:#}{padding}")?;
            if !rest.is_empty() {
                write!(out, "{GAP}{rest}")?;
            }
            writeln!(out)?;
        }
        Ok(())
    }

    /// `cells`, from column `from` on, padded to `widths` and without
    /// trailing blanks.
    fn line(&self, cells: &[&str], widths: &[usize], from: usize) -> String {
        let mut line = String::new();
        let columns = self.headers.iter().zip(widths).skip(from);
        for (index, (cell, ((_, align), columns))) in cells.iter().zip(columns).enumerate() {
            if index > 0 {
                line.push_str(GAP);
            }
            let pad = pad(cell, *columns);
            match align {
                Align::Left => line.extend([*cell, &pad]),
                Align::Right => line.extend([pad.as_str(), cell]),
            }
        }
        line.trim_end().to_string()
    }

    /// The width of each column: its widest cell or header.
    fn widths(&self) -> Vec<usize> {
        self.headers
            .iter()
            .enumerate()
            .map(|(column, (header, _))| {
                let cells = self.rows.iter().filter_map(|row| row.get(column));
                cells
                    .map(|cell| width(cell))
                    .chain([width(header)])
                    .max()
                    .unwrap_or(0)
            })
            .collect()
    }
}

/// `text` cut to `room` columns, ending in `ellipsis` if it was cut.
fn cut(text: &str, room: Option<usize>, ellipsis: &str) -> String {
    let Some(room) = room.filter(|room| width(text) > *room) else {
        return text.to_string();
    };
    let mut left = room.saturating_sub(width(ellipsis));
    let kept: String = text
        .chars()
        .take_while(|c| {
            let size = unicode_width::UnicodeWidthChar::width(*c).unwrap_or(0);
            let fits = size <= left;
            left = left.saturating_sub(size);
            fits
        })
        .collect();
    format!("{}{ellipsis}", kept.trim_end())
}

/// `n` with its digits grouped in threes by spaces: `1 440`.
pub fn group(n: u64) -> String {
    let digits = n.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(' ');
        }
        grouped.push(digit);
    }
    grouped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::ui::caps::Caps;

    #[test]
    fn test_columns_align_and_trail_no_blanks() {
        let headers = [
            ("TARGET", Align::Left),
            ("CYCLES", Align::Right),
            ("NOTE", Align::Left),
        ];
        let mut table = Table::new(&headers);
        table.row(vec!["plant".into(), group(1440), String::new()]);
        table.row(vec!["fsw".into(), group(12), "slow".into()]);
        let mut out = Vec::new();
        table
            .write(&mut out, &Theme::new(&Caps::plain()), "    ")
            .expect("writes to a Vec");
        assert_eq!(
            String::from_utf8(out).expect("utf-8"),
            "    TARGET  CYCLES  NOTE\n    plant    1 440\n    fsw         12  slow\n"
        );
    }

    #[test]
    fn test_fit_cuts_the_last_column() {
        let headers = [("NAME", Align::Left), ("INPUTS", Align::Left)];
        let mut table = Table::new(&headers).fit(Some(20));
        table.row(vec!["pub".into(), "plant.sensors, plant.gps".into()]);
        table.row(vec!["nav".into(), "gps".into()]);
        let mut out = Vec::new();
        table
            .write(&mut out, &Theme::new(&Caps::plain()), "  ")
            .expect("writes to a Vec");
        assert_eq!(
            String::from_utf8(out).expect("utf-8"),
            "  NAME  INPUTS\n  pub   plant.senso…\n  nav   gps\n"
        );
    }

    #[test]
    fn test_wide_cells_pad_and_cut_by_columns() {
        let headers = [("NAME", Align::Left), ("KIND", Align::Left)];
        let mut table = Table::new(&headers).fit(Some(14));
        table.row(vec!["中文".into(), "x".into()]);
        table.row(vec!["ab".into(), "日本語の名前".into()]);
        let mut out = Vec::new();
        table
            .write(&mut out, &Theme::new(&Caps::plain()), "")
            .expect("writes to a Vec");
        let text = String::from_utf8(out).expect("utf-8");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[1], "中文  x", "{text}");
        assert!(lines.iter().all(|l| super::super::width(l) <= 14), "{text}");
        assert!(lines[2].ends_with('…'), "{text}");
    }

    #[test]
    fn test_group() {
        assert_eq!(group(0), "0");
        assert_eq!(group(999), "999");
        assert_eq!(group(1000), "1 000");
        assert_eq!(group(1234567), "1 234 567");
    }
}
