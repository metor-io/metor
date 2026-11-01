//! A borderless table: a muted header row, then aligned rows with the first
//! column bold.

use std::io::{self, Write};

use anstyle::Style;
pub use comfy_table::CellAlignment as Align;
use comfy_table::{ColumnConstraint, ContentArrangement, presets::NOTHING};

use super::theme::Theme;
use super::{glyph, width};

/// Columns of text, laid out when written.
#[derive(Debug)]
pub struct Table<'a> {
    headers: &'a [(&'a str, Align)],
    rows: Vec<Vec<String>>,
    /// The widest a line may be. The last column is cut to fit.
    width: Option<usize>,
}

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

    /// Writes the table, indented by `indent`, without trailing blanks.
    pub fn write(&self, out: &mut impl Write, theme: &Theme, indent: &str) -> io::Result<()> {
        let (muted, bold) = (theme.muted, theme.bold(Style::new()));
        let mut table = comfy_table::Table::new();
        table
            .load_preset(NOTHING)
            .set_truncation_indicator(glyph::ELLIPSIS)
            .set_header(
                self.headers
                    .iter()
                    .map(|(h, _)| format!("{muted}{h}{muted:#}")),
            );
        for row in &self.rows {
            let cells = row.iter().enumerate().map(|(column, cell)| match column {
                0 => format!("{bold}{cell}{bold:#}"),
                _ => cell.clone(),
            });
            table.add_row(cells);
        }
        if let Some(room) = self.width {
            let room = room.saturating_sub(width(indent));
            table
                .set_content_arrangement(ContentArrangement::Dynamic)
                .set_width(u16::try_from(room).unwrap_or(u16::MAX));
        }
        let last = self.headers.len().saturating_sub(1);
        for (index, (column, (_, align))) in table.column_iter_mut().zip(self.headers).enumerate() {
            column.set_padding((0, 2)).set_cell_alignment(*align);
            if index < last {
                column.set_constraint(ColumnConstraint::ContentWidth);
            }
        }
        for row in table.row_iter_mut() {
            row.max_height(1);
        }
        for line in table.lines() {
            writeln!(out, "{indent}{}", line.trim_end())?;
        }
        Ok(())
    }
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

    fn written(table: &Table<'_>, indent: &str) -> String {
        let mut out = Vec::new();
        table
            .write(&mut out, &Theme::new(&Caps::plain()), indent)
            .expect("writes to a Vec");
        String::from_utf8(out).expect("utf-8")
    }

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
        assert_eq!(
            written(&table, "    "),
            "    TARGET  CYCLES  NOTE\n    plant    1 440\n    fsw         12  slow\n"
        );
    }

    #[test]
    fn test_fit_cuts_the_last_column() {
        let headers = [("NAME", Align::Left), ("INPUTS", Align::Left)];
        let mut table = Table::new(&headers).fit(Some(20));
        table.row(vec!["pub".into(), "plant.sensors, plant.gps".into()]);
        table.row(vec!["nav".into(), "gps".into()]);
        let text = written(&table, "  ");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "  NAME  INPUTS", "{text}");
        assert!(
            lines[1].starts_with("  pub   plant") && lines[1].ends_with('…'),
            "{text}"
        );
        assert_eq!(lines[2], "  nav   gps", "{text}");
        assert!(lines.iter().all(|l| width(l) <= 20), "{text}");
    }

    #[test]
    fn test_wide_cells_pad_and_cut_by_columns() {
        let headers = [("NAME", Align::Left), ("KIND", Align::Left)];
        let mut table = Table::new(&headers).fit(Some(14));
        table.row(vec!["中文".into(), "x".into()]);
        table.row(vec!["ab".into(), "日本語の名前".into()]);
        let text = written(&table, "");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[1], "中文  x", "{text}");
        assert!(lines.iter().all(|l| width(l) <= 14), "{text}");
        assert!(lines[2].ends_with('…'), "{text}");
    }

    #[test]
    fn test_no_rows_is_a_header() {
        let headers = [("NAME", Align::Left)];
        assert_eq!(written(&Table::new(&headers), ""), "NAME\n");
    }

    #[test]
    fn test_group() {
        assert_eq!(group(0), "0");
        assert_eq!(group(999), "999");
        assert_eq!(group(1000), "1 000");
        assert_eq!(group(1234567), "1 234 567");
    }
}
