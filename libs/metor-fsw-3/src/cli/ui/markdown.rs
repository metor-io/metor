//! A Markdown subset for long-form help: headings, paragraphs, `-` lists,
//! fenced code, and inline code.

use std::io::{self, Write};

use super::theme::{Theme, Token};

/// One block of a document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block<'a> {
    Heading(&'a str),
    /// A paragraph's lines joined by spaces.
    Paragraph(String),
    List(Vec<String>),
    Code(Vec<&'a str>),
}

/// The blocks of `text`. Lines the subset does not know are paragraph text.
pub fn parse(text: &str) -> Vec<Block<'_>> {
    let mut blocks = Vec::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(heading) = trimmed.strip_prefix('#') {
            blocks.push(Block::Heading(heading.trim_start_matches('#').trim()));
        } else if trimmed.starts_with("```") {
            let code = lines
                .by_ref()
                .take_while(|line| !line.trim().starts_with("```"));
            blocks.push(Block::Code(code.collect()));
        } else if let Some(item) = trimmed.strip_prefix("- ") {
            let mut items = vec![item.to_string()];
            while let Some(next) = lines.next_if(|line| continues(line)) {
                match next.trim().strip_prefix("- ") {
                    Some(item) => items.push(item.to_string()),
                    None => join(items.last_mut(), next),
                }
            }
            blocks.push(Block::List(items));
        } else {
            let mut text = trimmed.to_string();
            while let Some(next) = lines.next_if(|line| continues(line) && !starts_block(line)) {
                join(Some(&mut text), next);
            }
            blocks.push(Block::Paragraph(text));
        }
    }
    blocks
}

fn continues(line: &str) -> bool {
    !line.trim().is_empty() && !line.trim().starts_with("```") && !line.trim().starts_with('#')
}

fn starts_block(line: &str) -> bool {
    line.trim().starts_with("- ")
}

fn join(text: Option<&mut String>, line: &str) {
    if let Some(text) = text {
        text.push(' ');
        text.push_str(line.trim());
    }
}

/// Writes `blocks` indented by two spaces, wrapping text to `width`.
pub fn render(
    out: &mut impl Write,
    blocks: &[Block<'_>],
    theme: &Theme,
    width: usize,
) -> io::Result<()> {
    for (index, block) in blocks.iter().enumerate() {
        if index > 0 {
            writeln!(out)?;
        }
        match block {
            Block::Heading(text) => {
                let style = theme.bold(theme.style(Token::Accent));
                writeln!(out, "  {style}{text}{style:#}")?;
            }
            Block::Paragraph(text) => wrap(out, text, theme, width, "  ", "  ")?,
            Block::List(items) => {
                let first = format!("  {} ", theme.glyphs.bullet);
                for item in items {
                    wrap(out, item, theme, width, &first, "    ")?;
                }
            }
            Block::Code(lines) => {
                let muted = theme.style(Token::Muted);
                for line in lines {
                    writeln!(out, "    {muted}{line}{muted:#}")?;
                }
            }
        }
    }
    Ok(())
}

/// A run of text, styled as code or not.
type Piece<'a> = (&'a str, bool);

/// The words of `text`: whitespace outside code spans separates them, and a
/// word may mix code and plain pieces, as in `` `nav`, ``.
fn words(text: &str) -> Vec<Vec<Piece<'_>>> {
    let mut words = Vec::new();
    let mut word: Vec<Piece<'_>> = Vec::new();
    for (index, span) in text.split('`').enumerate() {
        let code = index % 2 == 1;
        if code {
            word.push((span, true));
            continue;
        }
        let mut parts = span.split(char::is_whitespace).peekable();
        while let Some(part) = parts.next() {
            if !part.is_empty() {
                word.push((part, false));
            }
            if parts.peek().is_some() && !word.is_empty() {
                words.push(core::mem::take(&mut word));
            }
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

fn code_pieces(word: &[Piece<'_>]) -> usize {
    word.iter().filter(|(_, code)| *code).count()
}

fn word_width(word: &[Piece<'_>]) -> usize {
    word.iter().map(|(text, _)| text.chars().count()).sum()
}

/// Writes `text` wrapped to `width`, its first line after `first` and the
/// rest after `rest`. A word wider than the line gets a line of its own.
/// Without colour, code keeps its backticks.
fn wrap(
    out: &mut impl Write,
    text: &str,
    theme: &Theme,
    width: usize,
    first: &str,
    rest: &str,
) -> io::Result<()> {
    let code = theme.style(Token::Accent);
    let marked = code == anstyle::Style::new();
    let mut column = first.chars().count();
    write!(out, "{first}")?;
    let mut fresh = true;
    for word in words(text) {
        let marks = if marked { 2 * code_pieces(&word) } else { 0 };
        let size = word_width(&word) + marks;
        if !fresh && column + 1 + size > width {
            write!(out, "\n{rest}")?;
            column = rest.chars().count();
            fresh = true;
        }
        if !fresh {
            write!(out, " ")?;
            column += 1;
        }
        for (piece, is_code) in word {
            match (is_code, marked) {
                (true, true) => write!(out, "`{piece}`")?,
                (true, false) => write!(out, "{code}{piece}{code:#}")?,
                (false, _) => write!(out, "{piece}")?,
            }
        }
        column += size;
        fresh = false;
    }
    writeln!(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::ui::caps::Caps;

    const TEXT: &str = "## M0201 The target file raised an exception

Python raised while evaluating the target file,
so there is no deployment.

- a typo in `target.py`, or
- an import that failed:
  the module is not installed

```python
fsw = Target(cycle_rate=100.0)
```
";

    #[test]
    fn test_parse_each_block() {
        assert_eq!(
            parse(TEXT),
            [
                Block::Heading("M0201 The target file raised an exception"),
                Block::Paragraph(
                    "Python raised while evaluating the target file, so there is no deployment."
                        .into()
                ),
                Block::List(vec![
                    "a typo in `target.py`, or".into(),
                    "an import that failed: the module is not installed".into(),
                ]),
                Block::Code(vec!["fsw = Target(cycle_rate=100.0)"]),
            ]
        );
    }

    #[test]
    fn test_parse_edges() {
        assert!(parse("").is_empty());
        assert_eq!(parse("```\nopen"), [Block::Code(vec!["open"])]);
        assert_eq!(
            parse("text\n- item"),
            [
                Block::Paragraph("text".into()),
                Block::List(vec!["item".into()])
            ]
        );
    }

    #[test]
    fn test_words_keep_code_spans_whole() {
        let words = words("set `a b`, then go");
        assert_eq!(
            words,
            [
                vec![("set", false)],
                vec![("a b", true), (",", false)],
                vec![("then", false)],
                vec![("go", false)],
            ]
        );
    }

    fn rendered(width: usize) -> String {
        let mut out = Vec::new();
        render(&mut out, &parse(TEXT), &Theme::new(&Caps::plain()), width)
            .expect("writes to a Vec");
        String::from_utf8(out).expect("utf-8")
    }

    #[test]
    fn test_render_wraps_to_the_width() {
        insta::assert_snapshot!(rendered(40));
        assert!(
            rendered(40)
                .lines()
                .skip(1)
                .all(|line| line.chars().count() <= 40),
            "headings do not wrap"
        );
    }

    #[test]
    fn test_render_gives_a_long_word_its_own_line() {
        let mut out = Vec::new();
        let blocks = [Block::Paragraph("a verylongword b".into())];
        render(&mut out, &blocks, &Theme::new(&Caps::plain()), 8).expect("writes to a Vec");
        assert_eq!(
            String::from_utf8(out).expect("utf-8"),
            "  a\n  verylongword\n  b\n"
        );
    }
}
