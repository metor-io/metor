//! `metor explain` prints the long form of a diagnostic code.

use std::io::{self, Write};
use std::path::PathBuf;

use serde_json::json;

use super::run::RunError;
use super::ui::event::Event;
use super::ui::markdown;
use super::ui::table::{Align, Table};
use super::ui::theme::{Theme, Token};
use super::ui::{Format, Ui};

/// One `## M0201 Title` section per code.
const CODES: &str = include_str!("codes.md");

/// The widest a wrapped line gets, for reading.
const MAX_WIDTH: usize = 100;

/// The arguments to `metor explain`.
#[derive(clap::Args, Debug, Default)]
pub struct Explain {
    /// A code, such as `M0201` or `201`. Without one, every code is listed.
    pub code: Option<String>,
}

/// One code's text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Section<'a> {
    pub code: &'a str,
    pub title: &'a str,
    pub body: &'a str,
}

/// The sections of `text`, each from its `## ` line to the next.
pub fn sections(text: &str) -> impl Iterator<Item = Section<'_>> {
    text.split("\n## ").filter_map(|chunk| {
        let chunk = chunk.strip_prefix("## ").unwrap_or(chunk);
        let (head, body) = chunk.split_once('\n').unwrap_or((chunk, ""));
        let (code, title) = head.split_once(' ')?;
        code.starts_with('M').then_some(Section {
            code,
            title: title.trim(),
            body: body.trim(),
        })
    })
}

/// `M0201` from `m0201`, `0201`, or `201`. Anything else, uppercased.
pub fn normalize(code: &str) -> String {
    let code = code.trim().to_ascii_uppercase();
    let digits = code.strip_prefix('M').unwrap_or(&code);
    match digits.parse::<u16>() {
        Ok(number) if digits.bytes().all(|b| b.is_ascii_digit()) => format!("M{number:04}"),
        _ => code,
    }
}

/// Prints the code `args` names, or lists every code.
pub fn explain(args: Explain, ui: &Ui) -> Result<(), RunError> {
    let Some(code) = args.code else {
        return list(ui);
    };
    let code = normalize(&code);
    let section = sections(CODES)
        .find(|section| section.code == code)
        .ok_or_else(|| RunError::NoSuchCode {
            code: code.clone(),
            known: sections(CODES).map(|s| s.code.to_string()).collect(),
        })?;
    if ui.format == Format::Json {
        let value = json!({"code": section.code, "title": section.title, "body": section.body});
        ui.emit(Event::Data("explain", &value));
        return Ok(());
    }
    let theme = Theme::new(&ui.stdout_caps());
    let width = ui.stdout_width().unwrap_or(MAX_WIDTH).min(MAX_WIDTH);
    write_stdout(|out| render(out, &section, &theme, width))
}

fn list(ui: &Ui) -> Result<(), RunError> {
    if ui.format == Format::Json {
        let codes: Vec<_> = sections(CODES)
            .map(|s| json!({"code": s.code, "title": s.title}))
            .collect();
        ui.emit(Event::Data("codes", &json!(codes)));
        return Ok(());
    }
    let theme = Theme::new(&ui.stdout_caps());
    let width = ui.stdout_width();
    write_stdout(|out| render_list(out, sections(CODES), &theme, width))
}

fn write_stdout(
    write: impl FnOnce(&mut io::StdoutLock<'static>) -> io::Result<()>,
) -> Result<(), RunError> {
    write(&mut io::stdout().lock()).map_err(|source| RunError::Io {
        path: PathBuf::from("stdout"),
        source,
    })
}

/// The code and title, then the body.
pub fn render(
    out: &mut impl Write,
    section: &Section<'_>,
    theme: &Theme,
    width: usize,
) -> io::Result<()> {
    let code = theme.bold(theme.style(Token::Error));
    let title = theme.bold(theme.style(Token::Text));
    writeln!(
        out,
        "  {code}{}{code:#}  {title}{}{title:#}",
        section.code, section.title
    )?;
    writeln!(out)?;
    markdown::render(out, &markdown::parse(section.body), theme, width)
}

const LIST_COLUMNS: [(&str, Align); 2] = [("CODE", Align::Left), ("TITLE", Align::Left)];

/// A table of every code and its title.
pub fn render_list<'a>(
    out: &mut impl Write,
    sections: impl Iterator<Item = Section<'a>>,
    theme: &Theme,
    width: Option<usize>,
) -> io::Result<()> {
    let mut table = Table::new(&LIST_COLUMNS).fit(width);
    for section in sections {
        table.row(vec![section.code.into(), section.title.into()]);
    }
    table.write(out, theme, "  ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::ui::caps::Caps;

    /// Every `"M0123"` literal in `source`.
    fn codes_in(source: &str) -> Vec<&str> {
        let mut codes: Vec<&str> = source
            .match_indices("\"M")
            .filter_map(|(at, _)| source.get(at + 1..at + 6))
            .filter(|code| code[1..].bytes().all(|b| b.is_ascii_digit()))
            .collect();
        codes.sort_unstable();
        codes.dedup();
        codes
    }

    #[test]
    fn test_every_code_has_a_section_and_every_section_a_code() {
        let used = codes_in(include_str!("diagnostic.rs"));
        let documented: Vec<&str> = sections(CODES).map(|s| s.code).collect();
        assert!(used.len() > 50, "{used:?}");
        let missing: Vec<_> = used
            .iter()
            .filter(|code| !documented.contains(code))
            .collect();
        assert!(missing.is_empty(), "codes with no text: {missing:?}");
        let unused: Vec<_> = documented
            .iter()
            .filter(|code| !used.contains(code))
            .collect();
        assert!(unused.is_empty(), "texts with no code: {unused:?}");
        let mut sorted = documented.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted, documented, "sections are in order, once each");
        assert!(sections(CODES).all(|s| !s.title.is_empty() && !s.body.is_empty()));
    }

    #[test]
    fn test_sections_split_on_headings() {
        let text = "## M0001 One\n\nFirst.\n\n## M0002 Two\nSecond.\n## nope\n";
        let found: Vec<_> = sections(text).collect();
        assert_eq!(
            found,
            [
                Section {
                    code: "M0001",
                    title: "One",
                    body: "First."
                },
                Section {
                    code: "M0002",
                    title: "Two",
                    body: "Second."
                },
            ]
        );
        assert_eq!(sections("").count(), 0);
    }

    #[test]
    fn test_normalize() {
        assert_eq!(normalize("M0201"), "M0201");
        assert_eq!(normalize(" m0201 "), "M0201");
        assert_eq!(normalize("201"), "M0201");
        assert_eq!(normalize("0201"), "M0201");
        assert_eq!(normalize("E0308"), "E0308");
        assert_eq!(normalize("M+201"), "M+201");
    }

    #[test]
    fn test_unknown_code_suggests_the_closest() {
        let ui = Ui::silent();
        let args = Explain {
            code: Some("M0299".into()),
        };
        let error = explain(args, &ui).expect_err("M0299 is not a code");
        let report = crate::cli::diagnostic::Report::new(&error);
        let help = miette::Diagnostic::help(&report).map(|h| h.to_string());
        assert!(
            help.is_some_and(|h| h.contains("metor explain")),
            "{report:?}"
        );
    }

    #[test]
    fn test_render_section() {
        let section = sections(CODES)
            .find(|s| s.code == "M0304")
            .expect("M0304 is documented");
        let mut out = Vec::new();
        render(&mut out, &section, &Theme::new(&Caps::plain()), 60).expect("writes to a Vec");
        let text = String::from_utf8(out).expect("utf-8");
        assert!(text.starts_with("  M0304  "), "{text}");
        assert!(
            text.lines()
                .all(|line| line.chars().count() <= 60 || line.starts_with("    "))
        );
    }
}
