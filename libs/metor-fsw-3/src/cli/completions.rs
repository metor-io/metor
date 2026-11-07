//! Shell completion of target and system names, from a cache the last
//! `prepare` wrote. `clap_complete` does the rest: `COMPLETE=fish metor`
//! prints the script that registers it.
//!
//! Completion never evaluates Python, so it stays fast. The cache is
//! `.metor/names` beside the target file: one `target NAME` or `system NAME`
//! line per name.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use clap_complete::engine::{
    ArgValueCompleter, CompletionCandidate, PathCompleter, ValueCompleter,
};

use super::run::find_target;
use super::show;
use crate::deploy::ResolvedDeployment;

/// A cached name's kind.
pub const TARGET: &str = "target";
pub const SYSTEM: &str = "system";

/// The cache of `config`'s names: its groups and targets, then every name
/// `show` takes.
pub fn cache(config: &ResolvedDeployment) -> String {
    let targets = config
        .groups()
        .into_iter()
        .chain(config.namespaces())
        .filter(|ns| !ns.is_empty())
        .map(|ns| format!("{TARGET} {ns}\n"));
    let names = show::names(config).into_iter();
    let systems = names
        .filter(|name| !config.namespaces().contains(&name.as_str()))
        .filter(|name| !config.groups().contains(&name.as_str()))
        .map(|name| format!("{SYSTEM} {name}\n"));
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
pub fn candidates<'a>(cache: &'a str, kinds: &[&str], prefix: &str) -> Vec<&'a str> {
    cache
        .lines()
        .filter_map(|line| line.split_once(' '))
        .filter(|(kind, name)| kinds.contains(kind) && name.starts_with(prefix))
        .map(|(_, name)| name)
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

/// Completes the cached names of `kinds`, and target files when `files`.
pub fn names(kinds: &'static [&'static str], files: bool) -> ArgValueCompleter {
    ArgValueCompleter::new(move |current: &OsStr| {
        let cache = nearest_cache();
        let names = candidates(&cache, kinds, &current.to_string_lossy())
            .into_iter()
            .map(CompletionCandidate::new);
        let files = files.then(|| target_files().complete(current));
        names.chain(files.into_iter().flatten()).collect::<Vec<_>>()
    })
}

/// Completes a target file.
pub fn files() -> ArgValueCompleter {
    ArgValueCompleter::new(target_files())
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

#[cfg(test)]
mod tests {
    use super::*;

    const CACHE: &str =
        "target plant\ntarget fsw\nsystem plant.pub\nsystem fsw.nav\nbad\nother x\n";

    #[test]
    fn test_candidates_filter_by_kind_and_prefix() {
        assert_eq!(candidates(CACHE, &[TARGET], ""), ["plant", "fsw"]);
        assert_eq!(
            candidates(CACHE, &[TARGET, SYSTEM], "p"),
            ["plant", "plant.pub"]
        );
        assert_eq!(candidates(CACHE, &[SYSTEM], "fsw."), ["fsw.nav"]);
        assert!(candidates("", &[TARGET], "").is_empty());
        assert!(candidates(CACHE, &[TARGET], "zz").is_empty());
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
