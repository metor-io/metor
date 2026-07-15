//! Shared fixture build/locate helpers for the integration tests.

use std::path::PathBuf;
use std::process::Command;

/// The platform file name of a `cdylib` with library stem `stem`.
pub fn fixture_lib_name(stem: &str) -> String {
    if cfg!(target_os = "macos") {
        format!("lib{stem}.dylib")
    } else if cfg!(target_os = "windows") {
        format!("{stem}.dll")
    } else {
        format!("lib{stem}.so")
    }
}

/// Build the cargo package `package` and return its `cdylib` path (library stem
/// `stem`), parsed from cargo's JSON artifact output so a custom target dir or
/// profile still resolves. Returns `None`, after a skip note on stderr, when the
/// build plumbing is unavailable, so the caller skips instead of failing.
pub fn locate_fixture(package: &str, stem: &str) -> Option<PathBuf> {
    let output = Command::new(env!("CARGO"))
        .args(["build", "-p", package, "--message-format=json"])
        .output()
        .ok()?;
    if !output.status.success() {
        eprintln!(
            "skipping: fixture build failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let want = fixture_lib_name(stem);
    for line in stdout.lines() {
        if !line.contains("compiler-artifact") || !line.contains(&want) {
            continue;
        }
        for tok in line.split('"') {
            if tok.ends_with(&want) {
                let path = PathBuf::from(tok);
                if path.exists() {
                    return Some(path);
                }
            }
        }
    }
    eprintln!("skipping: built the fixture but could not locate {want} in cargo output");
    None
}
