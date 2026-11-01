//! A borderless table: a muted header row, then aligned rows with the first
//! column bold, laid out by comfy-table.

use std::io::{self, Write};

use anstyle::Style;
pub use comfy_table::CellAlignment as Align;
use comfy_table::{ColumnConstraint, ContentArrangement, presets::NOTHING};

use super::glyph;
use super::theme::Theme;

/// Writes `rows` under `headers`, indented by `indent`, without trailing
/// blanks. The last column is cut so each line fits `width`.
pub fn write(
    out: &mut impl Write,
    headers: &[(&str, Align)],
    rows: impl IntoIterator<Item = Vec<String>>,
    theme: &Theme,
    width: Option<usize>,
    indent: &str,
) -> io::Result<()> {
    let (muted, bold) = (theme.muted, theme.bold(Style::new()));
    let mut table = comfy_table::Table::new();
    table
        .load_preset(NOTHING)
        .set_truncation_indicator(glyph::ELLIPSIS)
        .set_header(headers.iter().map(|(h, _)| format!("{muted}{h}{muted:#}")));
    for mut row in rows {
        if let Some(first) = row.first_mut() {
            *first = format!("{bold}{first}{bold:#}");
        }
        table.add_row(row);
    }
    if let Some(room) = width {
        let room = room.saturating_sub(super::width(indent));
        table
            .set_content_arrangement(ContentArrangement::Dynamic)
            .set_width(u16::try_from(room).unwrap_or(u16::MAX));
    }
    let last = headers.len().saturating_sub(1);
    for (index, (column, (_, align))) in table.column_iter_mut().zip(headers).enumerate() {
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

    fn written(
        headers: &[(&str, Align)],
        rows: &[&[&str]],
        width: Option<usize>,
        indent: &str,
    ) -> String {
        let rows = rows
            .iter()
            .map(|row| row.iter().map(|c| c.to_string()).collect());
        let mut out = Vec::new();
        write(
            &mut out,
            headers,
            rows,
            &Theme::new(&Caps::plain()),
            width,
            indent,
        )
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
        let rows: [&[&str]; 2] = [&["plant", "1 440", ""], &["fsw", "12", "slow"]];
        assert_eq!(
            written(&headers, &rows, None, "    "),
            "    TARGET  CYCLES  NOTE\n    plant    1 440\n    fsw         12  slow\n"
        );
    }

    #[test]
    fn test_fit_cuts_the_last_column() {
        let headers = [("NAME", Align::Left), ("INPUTS", Align::Left)];
        let rows: [&[&str]; 2] = [&["pub", "plant.sensors, plant.gps"], &["nav", "gps"]];
        let text = written(&headers, &rows, Some(20), "  ");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "  NAME  INPUTS", "{text}");
        assert!(
            lines[1].starts_with("  pub   plant") && lines[1].ends_with('…'),
            "{text}"
        );
        assert_eq!(lines[2], "  nav   gps", "{text}");
        assert!(lines.iter().all(|l| super::super::width(l) <= 20), "{text}");
    }

    #[test]
    fn test_wide_cells_pad_and_cut_by_columns() {
        let headers = [("NAME", Align::Left), ("KIND", Align::Left)];
        let rows: [&[&str]; 2] = [&["中文", "x"], &["ab", "日本語の名前"]];
        let text = written(&headers, &rows, Some(14), "");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[1], "中文  x", "{text}");
        assert!(lines.iter().all(|l| super::super::width(l) <= 14), "{text}");
        assert!(lines[2].ends_with('…'), "{text}");
    }

    #[test]
    fn test_no_rows_is_a_header() {
        let headers = [("NAME", Align::Left)];
        assert_eq!(written(&headers, &[], None, ""), "NAME\n");
    }

    #[test]
    fn test_group() {
        assert_eq!(group(0), "0");
        assert_eq!(group(999), "999");
        assert_eq!(group(1000), "1 000");
        assert_eq!(group(1234567), "1 234 567");
    }
}
