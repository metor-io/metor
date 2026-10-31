//! Shell completion: the registration script, and target and system names
//! from a cache the last `prepare` wrote.
//!
//! Completion never evaluates Python, so it stays fast. The cache is
//! `.metor/names` beside the target file: one `target NAME` or `system NAME`
//! line per name.

use std::ffi::OsStr;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use clap_complete::engine::{
    ArgValueCompleter, CompletionCandidate, PathCompleter, ValueCompleter,
};
use clap_complete::env::Shells;

use super::run::{RunError, find_target};
use super::show;
use crate::deploy::ResolvedDeployment;

/// The environment variable the registration script sets to ask for
/// candidates.
pub const VAR: &str = "COMPLETE";

/// The arguments to `metor completions`.
#[derive(clap::Args, Debug)]
pub struct Completions {
    /// The shell to write the script for.
    #[arg(value_enum)]
    pub shell: Shell,
}

/// A shell `clap_complete` registers with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
    Elvish,
    Powershell,
}

/// Writes the script that registers `metor`'s completions with `args.shell`.
pub fn completions(args: Completions) -> Result<(), RunError> {
    let mut out = io::stdout().lock();
    registration(&mut out, args.shell).map_err(|source| RunError::Io {
        path: PathBuf::from("stdout"),
        source,
    })
}

pub fn registration(out: &mut impl Write, shell: Shell) -> io::Result<()> {
    let name = match shell {
        Shell::Bash => "bash",
        Shell::Zsh => "zsh",
        Shell::Fish => "fish",
        Shell::Elvish => "elvish",
        Shell::Powershell => "powershell",
    };
    let shells = Shells::builtins();
    let completer = shells
        .completer(name)
        .ok_or_else(|| io::Error::other(format!("no completer for {name}")))?;
    let mut buf = Vec::new();
    completer.write_registration(VAR, "metor", "metor", "metor", &mut buf)?;
    out.write_all(&buf)
}

/// What a cached name names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Target,
    System,
}

/// The cache of `config`'s names: its targets, then every name `show`
/// takes.
pub fn cache(config: &ResolvedDeployment) -> String {
    let targets = config
        .namespaces()
        .into_iter()
        .filter(|ns| !ns.is_empty())
        .map(|ns| format!("target {ns}\n"));
    let names = show::names(config).into_iter();
    let systems = names
        .filter(|name| !config.namespaces().contains(&name.as_str()))
        .map(|name| format!("system {name}\n"));
    targets.chain(systems).collect()
}

/// The cache file for the target file `target`.
pub fn cache_path(target: &Path) -> PathBuf {
    let dir = target.parent().unwrap_or(Path::new("."));
    dir.join(".metor").join("names")
}

/// Writes `config`'s names beside `target`. A failure only costs
/// completions, so it is ignored.
pub fn write_cache(target: &Path, config: &ResolvedDeployment) {
    let path = cache_path(target);
    let _ = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(&path, cache(config)));
}

/// The names of `kinds` in `cache` that start with `prefix`.
pub fn candidates<'a>(cache: &'a str, kinds: &[Kind], prefix: &str) -> Vec<&'a str> {
    cache
        .lines()
        .filter_map(|line| {
            let (kind, name) = line.split_once(' ')?;
            let kind = match kind {
                "target" => Kind::Target,
                "system" => Kind::System,
                _ => return None,
            };
            (kinds.contains(&kind) && name.starts_with(prefix)).then_some(name)
        })
        .collect()
}

/// The cache of the nearest target file, or nothing.
fn nearest_cache() -> String {
    let target = std::env::current_dir()
        .ok()
        .and_then(|cwd| find_target(&cwd));
    target
        .and_then(|target| std::fs::read_to_string(cache_path(&target)).ok())
        .unwrap_or_default()
}

fn complete(current: &OsStr, kinds: &[Kind], files: bool) -> Vec<CompletionCandidate> {
    let prefix = current.to_string_lossy();
    let cache = nearest_cache();
    let names = candidates(&cache, kinds, &prefix)
        .into_iter()
        .map(CompletionCandidate::new);
    let files = match files {
        true => target_files().complete(current),
        false => Vec::new(),
    };
    names.chain(files).collect()
}

/// Directories, and files `metor` reads: `.py` and `.json`.
fn target_files() -> PathCompleter {
    PathCompleter::any().filter(|path| {
        path.is_dir()
            || path
                .extension()
                .is_some_and(|ext| ext == "py" || ext == "json")
    })
}

/// Completes a target's namespace.
pub fn targets() -> ArgValueCompleter {
    ArgValueCompleter::new(|current: &OsStr| complete(current, &[Kind::Target], false))
}

/// Completes a target file, or a target or system name.
pub fn files_or_names() -> ArgValueCompleter {
    ArgValueCompleter::new(|current: &OsStr| complete(current, &[Kind::Target, Kind::System], true))
}

/// Completes a target file, or a target name.
pub fn files_or_targets() -> ArgValueCompleter {
    ArgValueCompleter::new(|current: &OsStr| complete(current, &[Kind::Target], true))
}

/// Completes a target file.
pub fn files() -> ArgValueCompleter {
    ArgValueCompleter::new(target_files())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CACHE: &str =
        "target plant\ntarget fsw\nsystem plant.pub\nsystem fsw.nav\nbad\nother x\n";

    #[test]
    fn test_candidates_filter_by_kind_and_prefix() {
        assert_eq!(candidates(CACHE, &[Kind::Target], ""), ["plant", "fsw"]);
        assert_eq!(
            candidates(CACHE, &[Kind::Target, Kind::System], "p"),
            ["plant", "plant.pub"]
        );
        assert_eq!(candidates(CACHE, &[Kind::System], "fsw."), ["fsw.nav"]);
        assert!(candidates("", &[Kind::Target], "").is_empty());
        assert!(candidates(CACHE, &[Kind::Target], "zz").is_empty());
    }

    #[test]
    fn test_registration_names_metor_for_every_shell() {
        for shell in [
            Shell::Bash,
            Shell::Zsh,
            Shell::Fish,
            Shell::Elvish,
            Shell::Powershell,
        ] {
            let mut out = Vec::new();
            registration(&mut out, shell).expect("writes to a Vec");
            let script = String::from_utf8(out).expect("utf-8");
            assert!(
                script.contains("metor") && script.contains(VAR),
                "{shell:?}: {script}"
            );
        }
    }

    #[test]
    fn test_cache_path_is_beside_the_target() {
        assert_eq!(
            cache_path(Path::new("proj/target.py")),
            Path::new("proj/.metor/names")
        );
        assert_eq!(
            cache_path(Path::new("target.py")),
            Path::new(".metor/names")
        );
    }
}
