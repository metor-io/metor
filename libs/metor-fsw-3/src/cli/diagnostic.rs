//! Turns a command's error into a diagnostic: a code, a message, a help
//! line, and the line of the target file it comes from.
//!
//! The runtime's error types stay free of presentation. Every code lives in
//! the tables here, one match per error type, so a new variant cannot go
//! without one.
//!
//! Codes group by family: `M01xx` names and flags, `M02xx` the target file
//! and the deployment, `M03xx` the coordinator build, `M04xx` packs and
//! cargo, `M05xx` the run.

use std::path::Path;

use miette::{LabeledSpan, NamedSource, SourceCode, SourceSpan};

use super::cargo::CargoError;
use super::pack_dev::PackDevError;
use super::run::RunError;
use crate::coordinator::{BuildError, Location};
use crate::deploy::{ConfigError, DeployError};
use crate::dl::PackError;

/// A code and an optional help line.
type Info = (&'static str, Option<String>);

/// An error ready to render.
#[derive(Debug)]
pub struct Report {
    code: &'static str,
    message: String,
    help: Option<String>,
    snippet: Option<Snippet>,
}

/// The line of the target file an error points at.
#[derive(Debug)]
struct Snippet {
    source: NamedSource<String>,
    span: SourceSpan,
}

impl Report {
    /// Builds the report for `error`. A located error reads its line from
    /// the file, which the target file names relative to the working
    /// directory. If the line cannot be read, the message keeps its location.
    pub fn new(error: &RunError) -> Self {
        let (code, help) = run_info(error);
        let located = located(error)
            .and_then(|(location, inner)| Some((Snippet::read(location, Path::new("."))?, inner)));
        let (snippet, message) = match located {
            Some((snippet, inner)) => (Some(snippet), inner),
            None => (None, error.to_string()),
        };
        Self {
            code,
            message,
            help,
            snippet,
        }
    }

    /// Reads `location`'s line relative to `base`.
    #[cfg(test)]
    fn in_dir(error: &RunError, base: &Path) -> Self {
        let mut report = Self::new(error);
        if let Some((location, inner)) = located(error)
            && let Some(snippet) = Snippet::read(location, base)
        {
            report.snippet = Some(snippet);
            report.message = inner;
        }
        report
    }
}

impl core::fmt::Display for Report {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Report {}

impl miette::Diagnostic for Report {
    fn code<'a>(&'a self) -> Option<Box<dyn core::fmt::Display + 'a>> {
        Some(Box::new(self.code))
    }

    fn help<'a>(&'a self) -> Option<Box<dyn core::fmt::Display + 'a>> {
        self.help
            .as_ref()
            .map(|help| Box::new(help) as Box<dyn core::fmt::Display>)
    }

    fn source_code(&self) -> Option<&dyn SourceCode> {
        self.snippet.as_ref().map(|s| &s.source as &dyn SourceCode)
    }

    fn labels(&self) -> Option<Box<dyn Iterator<Item = LabeledSpan> + '_>> {
        let span = self.snippet.as_ref()?.span;
        Some(Box::new(core::iter::once(LabeledSpan::new_with_span(
            None, span,
        ))))
    }
}

impl Snippet {
    /// Reads the line `location` names, spanning it without its indent.
    fn read(location: &Location, base: &Path) -> Option<Self> {
        let path = base.join(&location.file);
        let text = std::fs::read_to_string(&path).ok()?;
        let index = usize::try_from(location.line).ok()?.checked_sub(1)?;
        let start: usize = text.split_inclusive('\n').take(index).map(str::len).sum();
        let line = text.get(start..)?.lines().next()?;
        let indent = line.len() - line.trim_start().len();
        let span = SourceSpan::new((start + indent).into(), line.trim().len());
        Some(Self {
            source: NamedSource::new(&location.file, text),
            span,
        })
    }
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

fn run_info(error: &RunError) -> Info {
    match error {
        RunError::Config(error) => config_info(error),
        RunError::Deploy(error) => deploy_info(error),
        RunError::Build(error) => build_info(error),
        RunError::PackDev(error) => pack_dev_info(error),
        RunError::Pack(error) => pack_info(error),
        RunError::EpochWall => (
            "M0101",
            Some("pass --sim-dt, or run a simulated target".into()),
        ),
        RunError::NoTargetFile(_) => (
            "M0206",
            Some("run from the directory with target.py, or pass the target file".into()),
        ),
        RunError::NoProject(_) => (
            "M0209",
            Some("pass the target file, or run from a project directory".into()),
        ),
        RunError::NoDeployment => (
            "M0207",
            Some("create a `Target` or a `Deployment` in the target file".into()),
        ),
        RunError::Io { .. } => ("M0208", None),
        RunError::Python { .. } => ("M0201", Some("the traceback above shows where".into())),
        RunError::PythonPath(_) => ("M0202", None),
        RunError::Spawn(_) => (
            "M0203",
            Some("set $METOR_PYTHON or activate a virtualenv".into()),
        ),
        RunError::Latched(_) => (
            "M0501",
            Some(
                "a latched system stops for the rest of the run; its last log lines say why".into(),
            ),
        ),
        RunError::Children(_) => ("M0502", Some("each target's own error is above".into())),
        RunError::Interrupted => ("M0503", None),
    }
}

fn config_info(error: &ConfigError) -> Info {
    match error {
        ConfigError::NoSuchTarget { name, known } => ("M0102", Some(suggest(name, known))),
        ConfigError::SelectTarget(_) => ("M0103", None),
        ConfigError::Version { .. } => (
            "M0204",
            Some("install the metor-config that ships with this metor".into()),
        ),
        ConfigError::Decode(_) => ("M0205", None),
    }
}

fn deploy_info(error: &DeployError) -> Info {
    match error {
        DeployError::Located { source, .. } => deploy_info(source),
        DeployError::Build { source, .. } => build_info(source),
        DeployError::NoSuchTarget { .. } => ("M0210", None),
        DeployError::NoSuchLink { .. } => ("M0211", None),
        DeployError::NotAListener { .. } => (
            "M0212",
            Some("give the link a `Listen` transport with a fixed port".into()),
        ),
        DeployError::NoSuchPort { .. } => ("M0213", None),
        DeployError::PortTaken { .. } => ("M0214", Some("give each listener its own port".into())),
        DeployError::Cycle { .. } => ("M0215", None),
        DeployError::Unresolved { .. } => ("M0216", None),
        DeployError::UnresolvedDial { .. } => ("M0217", None),
        DeployError::Slot { .. } => ("M0218", None),
        DeployError::SlotContract { .. } => ("M0219", None),
        DeployError::ArtifactName { .. } => ("M0220", None),
    }
}

fn build_info(error: &BuildError) -> Info {
    let deterministic = || Some("pass --sim-dt and --epoch with --deterministic".into());
    match error {
        BuildError::Located { source, .. } => build_info(source),
        BuildError::DeterministicWall => ("M0301", deterministic()),
        BuildError::DeterministicZeroDt => ("M0302", deterministic()),
        BuildError::DeterministicEpoch => ("M0303", deterministic()),
        BuildError::DuplicateId { .. } => ("M0304", Some("give each system its own id".into())),
        BuildError::DuplicateInput { .. } => ("M0305", None),
        BuildError::DuplicateOutput { .. } => ("M0306", None),
        BuildError::UnknownType { .. } => (
            "M0307",
            Some("check that the target lists the pack this type comes from".into()),
        ),
        BuildError::UnknownSystem { .. } => ("M0308", None),
        BuildError::UnknownInput { .. } => ("M0309", None),
        BuildError::UnknownOutput { .. } => ("M0310", None),
        BuildError::IdMismatch { .. } => ("M0311", None),
        BuildError::UnsupportedFrameAlignment { .. } => ("M0312", None),
        BuildError::RingTooLarge { .. } => ("M0313", None),
        BuildError::DynamicFromLater { .. } => (
            "M0314",
            Some(
                "add the producer before the system that reads it, or read it through a loop"
                    .into(),
            ),
        ),
        BuildError::RecordConflict { .. } => ("M0315", None),
        BuildError::RecordIdConflict { .. } => ("M0316", None),
        BuildError::MessageIdConflict { .. } => ("M0317", None),
        BuildError::UnsupportedAdapter { .. } => ("M0318", None),
        BuildError::ReservedPort { .. } => ("M0319", None),
        BuildError::TypeDef { .. } => ("M0320", None),
        BuildError::DuplicateChannel { .. } => ("M0321", None),
        BuildError::ReservedChannelPort { .. } => ("M0322", None),
        BuildError::Ungranted { .. } => ("M0323", None),
        BuildError::DuplicateGrant { .. } => ("M0324", None),
        BuildError::UnknownGrant { .. } => ("M0325", None),
        BuildError::NoIoStep { .. } => ("M0326", Some("add an fsw.io system to the target".into())),
        BuildError::NoIoStepAfter { system } => {
            ("M0327", Some(format!("add fsw.io after `{system}`")))
        }
        BuildError::TooManyIoOps { .. } => ("M0328", None),
        BuildError::TooManyResources { .. } => ("M0329", None),
        BuildError::IoBackend { .. } => ("M0330", None),
        BuildError::UnknownResource { .. } => ("M0331", None),
        BuildError::Interface { .. } => ("M0332", None),
        BuildError::SimGrant { .. } => ("M0333", None),
        BuildError::UnknownHolder { .. } => ("M0334", None),
        BuildError::Open { .. } => ("M0335", None),
        BuildError::Def { .. } => ("M0336", None),
        BuildError::Make { .. } => ("M0337", None),
    }
}

fn pack_dev_info(error: &PackDevError) -> Info {
    match error {
        PackDevError::Cargo(error) => cargo_info(error),
        PackDevError::Pack(error) => pack_info(error),
        PackDevError::Read { .. } => ("M0401", None),
        PackDevError::Write { .. } => ("M0402", None),
        PackDevError::Toml { .. } => ("M0403", None),
        PackDevError::Schema { .. } => ("M0404", None),
        PackDevError::NameClash { .. } => ("M0405", Some("rename the param or the port".into())),
        PackDevError::Template(_) => ("M0406", None),
        PackDevError::AlreadyLoaded(_) => ("M0407", None),
        PackDevError::DescriptorMismatch => ("M0408", None),
    }
}

fn cargo_info(error: &CargoError) -> Info {
    match error {
        CargoError::Spawn(_) => ("M0410", Some("is cargo on your PATH?".into())),
        CargoError::Cargo { .. } => ("M0411", Some("cargo's errors are above".into())),
        CargoError::NoArtifact(..) => (
            "M0412",
            Some("set `crate-type = [\"cdylib\"]` in the pack's Cargo.toml".into()),
        ),
    }
}

fn pack_info(error: &PackError) -> Info {
    match error {
        PackError::Open(_) => ("M0420", None),
        PackError::MissingSymbol(_) => ("M0421", None),
        PackError::AbiMismatch { .. } => {
            ("M0422", Some("rebuild the pack against this metor".into()))
        }
        PackError::Decode => ("M0423", None),
        PackError::DescriptorTooLarge { .. } => ("M0424", None),
        PackError::DescriptorStatus(_) => ("M0425", None),
        PackError::Read(_) => ("M0426", None),
        PackError::Wasm(_) => ("M0427", None),
        PackError::Trap(_) => ("M0428", None),
    }
}

/// A help line naming the closest of `known` to `name`, then all of them.
fn suggest(name: &str, known: &[String]) -> String {
    let closest = known
        .iter()
        .map(|candidate| (strsim::damerau_levenshtein(name, candidate), candidate))
        .filter(|(distance, candidate)| *distance <= candidate.len().div_ceil(3))
        .min_by_key(|(distance, _)| *distance);
    let list = known.join(", ");
    match closest {
        Some((_, candidate)) => format!("did you mean `{candidate}`?  targets: {list}"),
        None => format!("targets: {list}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_suggest_names_the_closest_target() {
        let known = ["plant", "fsw", "gw"].map(String::from);
        assert_eq!(
            suggest("fws", &known),
            "did you mean `fsw`?  targets: plant, fsw, gw"
        );
        assert_eq!(
            suggest("palnt", &known),
            "did you mean `plant`?  targets: plant, fsw, gw"
        );
        assert_eq!(suggest("zzz", &known), "targets: plant, fsw, gw");
        assert_eq!(suggest("x", &[]), "targets: ");
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
        let report = Report::in_dir(&error, dir.path());
        assert_eq!(report.to_string(), "system id `nav` is used twice");
        assert_eq!(report.code, "M0304");
        let snippet = report.snippet.expect("the line exists");
        assert_eq!(
            snippet.span,
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
            Report::in_dir(&error, dir.path()).snippet.is_some()
        };
        assert!(at("t.py", 1));
        let missing = RunError::Build(BuildError::Located {
            location: Location {
                file: "gone.py".into(),
                line: 3,
            },
            source: Box::new(BuildError::DeterministicWall),
        });
        let report = Report::in_dir(&missing, dir.path());
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
        let report = Report::in_dir(&error, dir.path());
        let mut out = Vec::new();
        Human::new(&mut out, Caps::plain()).event(&Event::Diagnostic(&report));
        insta::assert_snapshot!(String::from_utf8(out).expect("utf-8"));
    }
}
