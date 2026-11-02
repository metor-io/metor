//! `metor new` writes a pack crate, or a target project that runs as is.
//!
//! The metor dependencies point at the source tree this binary was built
//! from when it still exists, so a project made inside a checkout builds
//! against it. Otherwise they point at the git repository.

use std::path::{Component, Path, PathBuf};

use serde_json::json;

use super::run::{RunError, io, phased};
use super::ui::event::{Event, Verb};
use super::ui::{Format, Ui};

/// The metor-fsw-3 crate this binary was built from.
const SOURCE: &str = env!("CARGO_MANIFEST_DIR");

/// Where `SOURCE` lives in the repository.
const REPO: &str = "https://github.com/metor-io/metor";
const REPO_DIR: &str = "libs/metor-fsw-3";

/// The arguments to `metor new`.
#[derive(clap::Args, Debug)]
pub struct New {
    /// What to make.
    #[arg(value_enum)]
    pub kind: Kind,
    /// The directory to create. Its last component names the project.
    pub path: PathBuf,
}

/// What `new` makes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Kind {
    /// A pack crate with one system.
    Pack,
    /// A target project, with a pack, that `metor run` runs.
    Target,
}

/// A file to write, relative to the new directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct File {
    pub path: PathBuf,
    pub text: String,
}

/// Where the metor dependencies come from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// The metor-fsw-3 crate directory of a checkout.
    Tree(PathBuf),
    Git,
}

impl Source {
    /// The source tree, if it is still where this binary was built.
    pub fn detect() -> Self {
        match Path::new(SOURCE).canonicalize() {
            Ok(tree) if tree.join("python/metor-config").is_dir() => Self::Tree(tree),
            _ => Self::Git,
        }
    }

    /// The metor-fsw-3 dependency, for a `Cargo.toml` in `dir`.
    fn cargo(&self, dir: &Path) -> String {
        match self {
            Self::Tree(tree) => format!("path = \"{}\"", relative(dir, tree)),
            Self::Git => format!("git = \"{REPO}\""),
        }
    }

    /// The `package` Python source, for a `pyproject.toml` in `dir`.
    fn python(&self, dir: &Path, package: &str) -> String {
        match self {
            Self::Tree(tree) => format!(
                "path = \"{}\", editable = true",
                relative(dir, &tree.join("python").join(package))
            ),
            Self::Git => {
                format!("git = \"{REPO}\", subdirectory = \"{REPO_DIR}/python/{package}\"")
            }
        }
    }
}

/// `to` from `from`, both absolute, with `/` between parts.
fn relative(from: &Path, to: &Path) -> String {
    let common = from
        .components()
        .zip(to.components())
        .take_while(|(a, b)| a == b)
        .count();
    let up = from.components().skip(common).map(|_| "..".to_string());
    let down = to
        .components()
        .skip(common)
        .map(|part| part.as_os_str().to_string_lossy().into_owned());
    let parts: Vec<String> = up.chain(down).collect();
    match parts.is_empty() {
        true => ".".into(),
        false => parts.join("/"),
    }
}

/// Words neither Python nor Rust takes as a name.
const RESERVED: &[&str] = &[
    "and", "as", "assert", "async", "await", "break", "class", "const", "continue", "crate", "def",
    "del", "dyn", "elif", "else", "enum", "except", "extern", "false", "finally", "fn", "for",
    "from", "global", "if", "impl", "import", "in", "is", "lambda", "let", "loop", "match", "mod",
    "move", "mut", "none", "nonlocal", "not", "or", "pass", "pub", "raise", "ref", "return",
    "self", "static", "struct", "super", "trait", "true", "try", "type", "unsafe", "use", "where",
    "while", "with", "yield",
];

/// `name` as an identifier, `-` becoming `_`, or why it cannot be one.
pub fn identifier(name: &str) -> Result<String, &'static str> {
    let mut chars = name.chars();
    if !chars.next().is_some_and(|c| c.is_ascii_lowercase()) {
        return Err("it must start with a lowercase letter");
    }
    if !chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_') {
        return Err("it may hold only lowercase letters, digits, `-`, and `_`");
    }
    if !name.ends_with(|c: char| c.is_ascii_alphanumeric()) {
        return Err("it must end with a letter or digit");
    }
    if TAKEN.contains(&name) {
        return Err("a metor package has that name");
    }
    let ident = name.replace('-', "_");
    match RESERVED.contains(&ident.as_str()) {
        true => Err("Python or Rust reserves it"),
        false => Ok(ident),
    }
}

/// The packages a new project depends on, which it cannot share a name with.
const TAKEN: &[&str] = &["metor-config", "metor-build", "metor-fsw-3"];

/// `template` with each `{{key}}` replaced by its value.
fn fill(template: &str, values: &[(&str, &str)]) -> String {
    values
        .iter()
        .fold(template.to_string(), |text, (key, value)| {
            text.replace(&format!("{{{{{key}}}}}"), value)
        })
}

/// The files of a pack named `dist` with id `id`, for a pack at the
/// absolute `dir`.
fn pack_files(dist: &str, id: &str, dir: &Path, source: &Source) -> Vec<File> {
    let module = dist.replace('-', "_");
    let values = [
        ("crate", dist),
        ("dist", dist),
        ("id", id),
        ("module", &module),
        ("metor_fsw", &source.cargo(dir)),
        ("metor_build", &source.python(dir, "metor-build")),
    ];
    let file = |path: &str, template: &str| File {
        path: path.into(),
        text: fill(template, &values),
    };
    vec![
        file("Cargo.toml", include_str!("new/pack.Cargo.toml.in")),
        file("pyproject.toml", include_str!("new/pack.pyproject.toml.in")),
        file("src/lib.rs", include_str!("new/pack.lib.rs.in")),
        file(".gitignore", include_str!("new/gitignore.in")),
    ]
}

/// A target project's files, with its pack under `systems/`, for a project
/// at the absolute `dir`.
fn target_files(name: &str, ident: &str, dir: &Path, source: &Source) -> Vec<File> {
    let pack_dist = format!("{name}-pack");
    let pack_module = format!("{ident}_pack");
    let values = [
        ("name", name),
        ("dist", name),
        ("pack_dist", &pack_dist),
        ("pack_module", &pack_module),
        ("metor_config", &source.python(dir, "metor-config")),
    ];
    let file = |path: &str, template: &str| File {
        path: path.into(),
        text: fill(template, &values),
    };
    let pack_dir = Path::new("systems").join(name);
    let pack = pack_files(&pack_dist, ident, &dir.join(&pack_dir), source)
        .into_iter()
        .filter(|file| file.path != Path::new(".gitignore"))
        .map(|file| File {
            path: pack_dir.join(file.path),
            ..file
        });
    let own = [
        file(
            "pyproject.toml",
            include_str!("new/target.pyproject.toml.in"),
        ),
        file("target.py", include_str!("new/target.py.in")),
        file(".gitignore", include_str!("new/gitignore.in")),
    ];
    own.into_iter().chain(pack).collect()
}

/// The files `kind` needs, for a project at the absolute `dir`.
pub fn files(kind: Kind, dir: &Path, source: &Source) -> Result<Vec<File>, RunError> {
    let name = dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ident = identifier(&name).map_err(|why| RunError::BadName {
        name: name.clone(),
        why,
    })?;
    Ok(match kind {
        Kind::Pack => pack_files(&name, &ident, dir, source),
        Kind::Target => target_files(&name, &ident, dir, source),
    })
}

/// Writes a new pack or target project at `args.path`.
pub fn new(args: New, ui: &Ui) -> Result<(), RunError> {
    let cwd = std::env::current_dir().map_err(io(Path::new(".")))?;
    let dir = physical(&normalize(&cwd.join(&args.path)));
    if dir.exists() {
        return Err(RunError::Exists(args.path));
    }
    let files = files(args.kind, &dir, &Source::detect())?;
    phased(ui, Verb::CREATE, &args.path.display().to_string(), || {
        write(&dir, &files).inspect_err(|_| {
            let _ = std::fs::remove_dir_all(&dir);
        })
    })?;
    report(ui, &args, &cwd, &files);
    Ok(())
}

/// `path` with `.` and `..` resolved by name, as `cd` would.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            part => out.push(part),
        }
    }
    out
}

/// `path` with its longest existing ancestor resolved through symlinks, so
/// paths relative to it hold for the OS too.
fn physical(path: &Path) -> PathBuf {
    let existing = path
        .ancestors()
        .find_map(|a| Some((a, a.canonicalize().ok()?)));
    match existing {
        Some((ancestor, real)) => real.join(path.strip_prefix(ancestor).unwrap_or(Path::new(""))),
        None => path.to_path_buf(),
    }
}

/// Writes every file under `dir`. On failure, `dir` is removed, since it
/// did not exist before.
fn write(dir: &Path, files: &[File]) -> Result<(), RunError> {
    for file in files {
        let path = dir.join(&file.path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(io(parent))?;
        }
        std::fs::write(&path, &file.text).map_err(io(&path))?;
    }
    Ok(())
}

/// What to do next, or the files written as JSON.
fn report(ui: &Ui, args: &New, cwd: &Path, files: &[File]) {
    if ui.format == Format::Json {
        let paths: Vec<String> = files.iter().map(|f| f.path.display().to_string()).collect();
        let value = json!({"path": args.path, "files": paths});
        ui.emit(Event::Data("new", &value));
        return;
    }
    let shown = args.path.display();
    let next = match args.kind {
        Kind::Target => format!("\n    cd {shown}\n    metor run\n"),
        Kind::Pack if cwd.join("pyproject.toml").is_file() => {
            let dist = args.path.file_name().unwrap_or_default().to_string_lossy();
            format!(
                "\n    add the pack to this project's pyproject.toml:\n\n    [tool.uv.sources]\n    {dist} = {{ path = \"{shown}\", editable = true }}\n"
            )
        }
        Kind::Pack => format!("\n    cd {shown}\n    metor pack dev\n"),
    };
    ui.emit(Event::Output(next.as_bytes()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_identifier() {
        assert_eq!(identifier("sat"), Ok("sat".into()));
        assert_eq!(identifier("my-sat-2"), Ok("my_sat_2".into()));
        assert!(identifier("").is_err());
        assert!(identifier("2sat").is_err());
        assert!(identifier("Sat").is_err());
        assert!(identifier("sat.v2").is_err());
        assert_eq!(identifier("import"), Err("Python or Rust reserves it"));
        assert!(identifier("my-sat-").is_err() && identifier("my_").is_err());
        assert!(identifier("metor-config").is_err());
    }

    #[test]
    fn test_relative() {
        let rel = |from: &str, to: &str| relative(Path::new(from), Path::new(to));
        assert_eq!(rel("/a/b/new", "/a/libs/fsw"), "../../libs/fsw");
        assert_eq!(rel("/a/libs/fsw/x", "/a/libs/fsw"), "..");
        assert_eq!(rel("/a", "/a"), ".");
    }

    #[test]
    fn test_fill_replaces_every_hole() {
        assert_eq!(
            fill("{{a}}-{{b}}-{{a}} {x}", &[("a", "1"), ("b", "2")]),
            "1-2-1 {x}"
        );
    }

    fn text<'a>(files: &'a [File], path: &str) -> &'a str {
        let file = files.iter().find(|f| f.path == Path::new(path));
        &file.unwrap_or_else(|| panic!("{path} is written")).text
    }

    #[test]
    fn test_target_files_from_a_tree() {
        let source = Source::Tree("/repo/libs/metor-fsw-3".into());
        let files =
            files(Kind::Target, Path::new("/repo/examples/my-sat"), &source).expect("a good name");
        assert!(
            files.iter().all(|f| !f.text.contains("{{")),
            "every hole filled"
        );
        let target = text(&files, "target.py");
        assert!(target.contains("from my_sat_pack import Hello"), "{target}");
        let project = text(&files, "pyproject.toml");
        assert!(
            project.contains("my-sat-pack = { path = \"systems/my-sat\""),
            "{project}"
        );
        assert!(
            project.contains("path = \"../../libs/metor-fsw-3/python/metor-config\""),
            "{project}"
        );
        let cargo = text(&files, "systems/my-sat/Cargo.toml");
        assert!(cargo.contains("name = \"my-sat-pack\""), "{cargo}");
        assert!(
            cargo.contains("path = \"../../../../libs/metor-fsw-3\""),
            "{cargo}"
        );
        assert!(text(&files, "systems/my-sat/pyproject.toml").contains("id = \"my_sat\""));
        assert!(
            !files
                .iter()
                .any(|f| f.path == Path::new("systems/my-sat/.gitignore"))
        );
    }

    #[test]
    fn test_pack_files_from_git() {
        let files =
            files(Kind::Pack, Path::new("/elsewhere/nav"), &Source::Git).expect("a good name");
        let cargo = text(&files, "Cargo.toml");
        assert!(
            cargo.contains("metor-fsw-3 = { git = \"https://github.com/metor-io/metor\""),
            "{cargo}"
        );
        let project = text(&files, "pyproject.toml");
        assert!(
            project.contains("subdirectory = \"libs/metor-fsw-3/python/metor-build\""),
            "{project}"
        );
        assert!(text(&files, "src/lib.rs").contains("The nav pack"));
    }

    #[test]
    fn test_bad_names_and_existing_directories_are_refused() {
        let bad = files(Kind::Pack, Path::new("/x/Nav"), &Source::Git).expect_err("uppercase");
        assert!(matches!(bad, RunError::BadName { .. }), "{bad}");
        let dir = tempfile::tempdir().expect("a temp dir");
        let args = New {
            kind: Kind::Pack,
            path: dir.path().to_path_buf(),
        };
        let exists = new(args, &Ui::silent()).expect_err("it exists");
        assert!(matches!(exists, RunError::Exists(_)), "{exists}");
    }

    #[test]
    fn test_physical_resolves_symlinked_ancestors() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let real = dir.path().canonicalize().expect("it exists");
        let link = real.join("link");
        std::os::unix::fs::symlink(&real, &link).expect("links");
        assert_eq!(physical(&link.join("new/pack")), real.join("new/pack"));
        assert_eq!(
            physical(Path::new("/no-such-root/x")),
            Path::new("/no-such-root/x")
        );
    }

    #[test]
    fn test_normalize() {
        assert_eq!(normalize(Path::new("/a/./b/../c")), Path::new("/a/c"));
    }
}
