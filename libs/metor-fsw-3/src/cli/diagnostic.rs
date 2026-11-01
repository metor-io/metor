//! Turns a command's error into a diagnostic: its code and help, which each
//! error variant names through `miette::Diagnostic`, the line of the target
//! file it comes from, and a link to the code's text in `codes.md`.
//!
//! Codes group by family: `M01xx` names and flags, `M02xx` the target file
//! and the deployment, `M03xx` the coordinator build, `M04xx` packs and
//! cargo, `M05xx` the run.

use std::path::Path;

use miette::{Diagnostic, LabeledSpan, MietteDiagnostic, NamedSource, SourceSpan};

use super::run::RunError;
use crate::coordinator::{BuildError, Location};
use crate::deploy::DeployError;

/// Where each code's text is published, one `## M0201` heading per code.
const CODES_URL: &str =
    "https://github.com/metor-io/metor/blob/main/libs/metor-fsw-3/src/cli/codes.md";

/// The diagnostic for `error`. A located error reads its line from the
/// file, which the target file names relative to the working directory. If
/// the line cannot be read, the message keeps its location.
pub fn report(error: &RunError) -> miette::Report {
    report_in(error, Path::new("."))
}

/// The diagnostic for `error`, reading its line relative to `base`.
fn report_in(error: &RunError, base: &Path) -> miette::Report {
    let code = error
        .code()
        .map(|code| code.to_string())
        .unwrap_or_default();
    let help = error.help().map(|help| help.to_string());
    let located =
        located(error).and_then(|(location, inner)| Some((snippet(location, base)?, inner)));
    match located {
        Some((snippet, inner)) => build(code, inner, help, Some(snippet)),
        None => build(code, error.to_string(), help, None),
    }
}

/// Rebuilds a diagnostic from the parts `--format json` writes, reading the
/// target file's line again if it names one.
pub fn from_parts(
    code: String,
    message: String,
    help: Option<String>,
    at: Option<Location>,
) -> miette::Report {
    let snippet = at.as_ref().and_then(|at| snippet(at, Path::new(".")));
    build(code, message, help, snippet)
}

/// A target file and the span of one of its lines.
type Snippet = (NamedSource<String>, SourceSpan);

fn build(
    code: String,
    message: String,
    help: Option<String>,
    snippet: Option<Snippet>,
) -> miette::Report {
    let url = format!("{CODES_URL}#{}", code.to_lowercase());
    let mut diagnostic = MietteDiagnostic::new(message).with_code(code).with_url(url);
    if let Some(help) = help {
        diagnostic = diagnostic.with_help(help);
    }
    let Some((source, span)) = snippet else {
        return miette::Report::new(diagnostic);
    };
    let label = LabeledSpan::new_with_span(None, span);
    miette::Report::new(diagnostic.with_label(label)).with_source_code(source)
}

/// The line `location` names, spanned without its indent.
fn snippet(location: &Location, base: &Path) -> Option<Snippet> {
    let text = std::fs::read_to_string(base.join(&location.file)).ok()?;
    let index = usize::try_from(location.line).ok()?.checked_sub(1)?;
    let start: usize = text.split_inclusive('\n').take(index).map(str::len).sum();
    let line = text.get(start..)?.lines().next()?;
    let indent = line.len() - line.trim_start().len();
    let span = SourceSpan::new((start + indent).into(), line.trim().len());
    Some((NamedSource::new(&location.file, text), span))
}

/// The location and message of the innermost located error in `error`.
fn located(error: &RunError) -> Option<(&Location, String)> {
    match error {
        RunError::Deploy(DeployError::Located { location, source }) => {
            Some((location, source.to_string()))
        }
        RunError::Build(BuildError::Located { location, source })
        | RunError::Deploy(DeployError::Build {
            source: BuildError::Located { location, source },
            ..
        }) => Some((location, source.to_string())),
        _ => None,
    }
}

/// The exit status for `error`.
pub fn exit_code(error: &RunError) -> i32 {
    match error {
        RunError::Latched(_) => 3,
        RunError::Children(_) => 4,
        RunError::Interrupted => 130,
        _ => 1,
    }
}

/// How many known names a help line lists in full.
const LISTED: usize = 8;

/// What a list of known names holds, and the command that lists them.
pub(crate) type Noun = (&'static str, &'static str);

pub(crate) const SHOW_NAMES: Noun = ("names", "metor show");
pub(crate) const SHOW_TARGETS: Noun = ("targets", "metor show");

/// A help line naming the closest of `known` to `name`, then all of them
/// if there are few.
pub(crate) fn suggest(name: &str, known: &[String], (noun, lister): Noun) -> String {
    let closest = known
        .iter()
        .map(|candidate| (strsim::damerau_levenshtein(name, candidate), candidate))
        .filter(|(distance, candidate)| *distance <= candidate.len().div_ceil(3))
        .min_by_key(|(distance, _)| *distance);
    let list = match known.len() {
        0..=LISTED => format!("{noun}: {}", known.join(", ")),
        n => format!("{n} {noun}; `{lister}` lists them"),
    };
    match closest {
        Some((_, candidate)) => format!("did you mean `{candidate}`?  {list}"),
        None => list,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The text behind each code's link: a `## M0201` heading, a bold title,
    /// then the body.
    const CODES: &str = include_str!("codes.md");

    /// The files whose error types name codes.
    const SOURCES: [&str; 7] = [
        include_str!("run.rs"),
        include_str!("pack_dev.rs"),
        include_str!("cargo.rs"),
        include_str!("../deploy/config.rs"),
        include_str!("../deploy/error.rs"),
        include_str!("../coordinator/error.rs"),
        include_str!("../dl/mod.rs"),
    ];

    /// Every `code(M0123)` in `SOURCES`, sorted.
    fn codes_used() -> Vec<&'static str> {
        let mut codes: Vec<&str> = SOURCES
            .iter()
            .flat_map(|source| {
                source
                    .match_indices("code(M")
                    .filter_map(|(at, _)| source.get(at + 5..at + 10))
            })
            .filter(|code| code[1..].bytes().all(|b| b.is_ascii_digit()))
            .collect();
        codes.sort_unstable();
        codes.dedup();
        codes
    }

    #[test]
    fn test_every_code_has_a_section_and_every_section_a_code() {
        let used = codes_used();
        let documented: Vec<&str> = CODES
            .lines()
            .filter_map(|l| l.strip_prefix("## "))
            .collect();
        assert!(used.len() > 50, "{used:?}");
        assert_eq!(
            documented, used,
            "codes.md holds each used code once, in order"
        );
    }

    #[test]
    fn test_each_section_has_a_title_and_fits() {
        let lines: Vec<&str> = CODES.lines().collect();
        for (at, line) in lines.iter().enumerate() {
            if line.starts_with("## ") {
                let title = lines.get(at + 2).copied().unwrap_or("");
                assert!(title.starts_with("**") && title.ends_with("**"), "{line}");
            }
        }
        assert_eq!(lines.iter().find(|l| l.chars().count() > 76), None);
    }

    #[test]
    fn test_suggest_names_the_closest_target() {
        let known = ["plant", "fsw", "gw"].map(String::from);
        let suggest = |name| suggest(name, &known, SHOW_TARGETS);
        assert_eq!(
            suggest("fws"),
            "did you mean `fsw`?  targets: plant, fsw, gw"
        );
        assert_eq!(
            suggest("palnt"),
            "did you mean `plant`?  targets: plant, fsw, gw"
        );
        assert_eq!(suggest("zzz"), "targets: plant, fsw, gw");
    }

    #[test]
    fn test_suggest_summarises_many_names() {
        let known: Vec<String> = (0..20).map(|i| format!("sys{i}")).collect();
        assert_eq!(
            suggest("sys1x", &known, SHOW_NAMES),
            "did you mean `sys1`?  20 names; `metor show` lists them"
        );
    }

    #[test]
    fn test_exit_codes() {
        assert_eq!(exit_code(&RunError::Latched("a".into())), 3);
        assert_eq!(exit_code(&RunError::Children("a".into())), 4);
        assert_eq!(exit_code(&RunError::Interrupted), 130);
        assert_eq!(exit_code(&RunError::EpochWall), 1);
    }

    #[test]
    fn test_located_error_reads_its_line() {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::write(dir.path().join("t.py"), "a = 1\n    nav = add(\"nav\")\n").expect("writes");
        let error = RunError::Build(BuildError::Located {
            location: Location {
                file: "t.py".into(),
                line: 2,
            },
            source: Box::new(BuildError::DuplicateId { id: "nav".into() }),
        });
        let report = report_in(&error, dir.path());
        assert_eq!(report.to_string(), "system id `nav` is used twice");
        assert_eq!(
            report.code().map(|c| c.to_string()).as_deref(),
            Some("M0304")
        );
        let label = report
            .labels()
            .and_then(|mut l| l.next())
            .expect("the line exists");
        assert_eq!(
            *label.inner(),
            SourceSpan::new(10.into(), "nav = add(\"nav\")".len())
        );
    }

    #[test]
    fn test_missing_file_or_line_drops_the_snippet() {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::write(dir.path().join("t.py"), "one line\n").expect("writes");
        let at = |file: &str, line| {
            let error = RunError::Build(BuildError::Located {
                location: Location {
                    file: file.into(),
                    line,
                },
                source: Box::new(BuildError::DeterministicWall),
            });
            report_in(&error, dir.path()).labels().is_some()
        };
        assert!(at("t.py", 1));
        let missing = RunError::Build(BuildError::Located {
            location: Location {
                file: "gone.py".into(),
                line: 3,
            },
            source: Box::new(BuildError::DeterministicWall),
        });
        let report = report_in(&missing, dir.path());
        assert!(report.to_string().starts_with("gone.py:3: "), "{report}");
        assert!(!at("t.py", 5));
        assert!(!at("t.py", 0));
        assert!(!at("gone.py", 1));
    }

    #[test]
    fn test_located_report_renders_its_line() {
        use crate::cli::ui::Reporter;
        use crate::cli::ui::caps::Caps;
        use crate::cli::ui::event::Event;
        use crate::cli::ui::human::Human;

        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::write(dir.path().join("t.py"), "a = 1\nfsw.io = 2\n").expect("writes");
        let error = RunError::Build(BuildError::Located {
            location: Location {
                file: "t.py".into(),
                line: 2,
            },
            source: Box::new(BuildError::NoIoStepAfter {
                system: "gps".into(),
            }),
        });
        let report = report_in(&error, dir.path());
        let mut out = Vec::new();
        Human::new(&mut out, Caps::plain()).event(&Event::Diagnostic(&*report));
        insta::assert_snapshot!(String::from_utf8(out).expect("utf-8"));
    }
}
