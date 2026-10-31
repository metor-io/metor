//! `metor doctor` checks the tools a project needs, one line each.
//!
//! Each check probes once and judges the result in a pure function, so the
//! judging is tested without the tools.

use std::ffi::OsStr;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::json;

use super::run::{RunError, find_target, interpreter, python_path};
use super::ui::event::Event;
use super::ui::theme::{Theme, Token};
use super::ui::{Format, Ui};
use crate::dl::WASM_TRIPLE;
use crate::pack::ABI_VERSION;

/// How a check went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Ok,
    /// Missing, but only some projects need it.
    Optional,
    Failed,
}

/// One tool's result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
    pub fix: Option<String>,
}

impl Check {
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Ok,
            detail: detail.into(),
            fix: None,
        }
    }

    fn failed(name: &'static str, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Failed,
            detail: detail.into(),
            fix: Some(fix.into()),
        }
    }
}

/// Runs every check and prints them. Fails if a required one failed.
pub fn doctor(ui: &Ui) -> Result<(), RunError> {
    let dir = project_dir()?;
    let checks = [
        python(&dir),
        metor_config(),
        uv(&dir),
        cargo(&dir),
        wasm32(&dir),
    ];
    if ui.format == Format::Json {
        let value = json!(checks.iter().map(to_json).collect::<Vec<_>>());
        ui.emit(Event::Data("doctor", &value));
    } else {
        let mut out = io::stdout().lock();
        render(&mut out, &checks, &Theme::new(&ui.stdout_caps())).map_err(|source| {
            RunError::Io {
                path: PathBuf::from("stdout"),
                source,
            }
        })?;
    }
    let failed: Vec<&str> = checks
        .iter()
        .filter(|check| check.status == Status::Failed)
        .map(|check| check.name)
        .collect();
    match failed.is_empty() {
        true => Ok(()),
        false => Err(RunError::Doctor(failed.join(", "))),
    }
}

/// The directory of the nearest `target.py`, or the working directory.
fn project_dir() -> Result<PathBuf, RunError> {
    let cwd = std::env::current_dir().map_err(|source| RunError::Io {
        path: PathBuf::from("."),
        source,
    })?;
    let found = find_target(&cwd).and_then(|target| target.parent().map(Path::to_path_buf));
    Ok(found.unwrap_or(cwd))
}

fn to_json(check: &Check) -> serde_json::Value {
    json!({
        "name": check.name,
        "ok": check.status != Status::Failed,
        "status": format!("{:?}", check.status).to_lowercase(),
        "detail": check.detail,
        "fix": check.fix,
    })
}

/// Writes one line per check, and each fix under its check.
pub fn render(out: &mut impl Write, checks: &[Check], theme: &Theme) -> io::Result<()> {
    let width = checks
        .iter()
        .map(|check| check.name.len())
        .max()
        .unwrap_or(0);
    let muted = theme.style(Token::Muted);
    for check in checks {
        let (glyph, token) = match check.status {
            Status::Ok => (theme.glyphs.ok, Token::Ok),
            Status::Optional => (theme.glyphs.warn, Token::Warn),
            Status::Failed => (theme.glyphs.fail, Token::Error),
        };
        let style = theme.style(token);
        let name = format!("{:width$}", check.name);
        writeln!(out, "  {style}{glyph}{style:#} {name}   {}", check.detail)?;
        if let Some(fix) = &check.fix {
            writeln!(out, "    {:width$}   {muted}{fix}{muted:#}", "")?;
        }
    }
    Ok(())
}

/// The first line `program args` prints in `dir`, if it runs and succeeds.
/// rustup picks a toolchain by directory, so a probe runs where the build
/// will.
fn first_line(dir: &Path, program: impl AsRef<OsStr>, args: &[&str]) -> Option<String> {
    let output = Command::new(program)
        .current_dir(dir)
        .args(args)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().next()?.trim().to_string();
    output.status.success().then_some(line)
}

/// `0.5.4` from `uv 0.5.4 (abc 2026-01-01)`.
fn version_number(line: &str) -> String {
    line.split_whitespace().nth(1).unwrap_or(line).to_string()
}

fn python(dir: &Path) -> Check {
    let python = interpreter();
    let shown = Path::new(&python).display().to_string();
    judge_python(first_line(dir, &python, &["--version"]), &shown)
}

fn judge_python(version: Option<String>, shown: &str) -> Check {
    match version {
        Some(version) => Check::ok(
            "python",
            format!("{}  {shown}", version.trim_start_matches("Python ")),
        ),
        None => Check::failed(
            "python",
            format!("{shown} did not run"),
            "set $METOR_PYTHON, or activate the project's virtualenv",
        ),
    }
}

fn metor_config() -> Check {
    let Ok(path) = python_path(&[]) else {
        return judge_metor_config(None);
    };
    let script = "import metor_config._generated as g; print(g.ABI_VERSION)";
    let output = Command::new(interpreter())
        .args(["-c", script])
        .env("PYTHONPATH", path)
        .output()
        .ok()
        .filter(|output| output.status.success());
    let abi = output.and_then(|output| String::from_utf8_lossy(&output.stdout).trim().parse().ok());
    judge_metor_config(abi)
}

fn judge_metor_config(abi: Option<u32>) -> Check {
    match abi {
        Some(ABI_VERSION) => Check::ok("metor-config", format!("ABI {ABI_VERSION}")),
        Some(found) => Check::failed(
            "metor-config",
            format!("ABI {found}, this metor speaks {ABI_VERSION}"),
            "install the metor-config that ships with this metor",
        ),
        None => Check::failed(
            "metor-config",
            "does not import",
            "uv add metor-config, or set $METOR_CONFIG_PY",
        ),
    }
}

fn uv(dir: &Path) -> Check {
    match first_line(dir, "uv", &["--version"]) {
        Some(version) => Check::ok("uv", version_number(&version)),
        None => Check {
            name: "uv",
            status: Status::Optional,
            detail: "not found; only uv projects need it".into(),
            fix: None,
        },
    }
}

fn cargo(dir: &Path) -> Check {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    match first_line(dir, cargo, &["--version"]) {
        Some(version) => Check::ok("cargo", version_number(&version)),
        None => Check::failed("cargo", "not found", "install Rust from https://rustup.rs"),
    }
}

/// Checks that the wasm32 standard library is installed for the toolchain
/// cargo uses in `dir`, which a pack built with `wasm = true` needs.
pub fn wasm32(dir: &Path) -> Check {
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let libdir = first_line(
        dir,
        rustc,
        &["--print", "target-libdir", "--target", WASM_TRIPLE],
    );
    judge_wasm32(libdir.as_deref().is_some_and(has_rlib))
}

fn has_rlib(dir: &str) -> bool {
    std::fs::read_dir(dir).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|entry| entry.path().extension().is_some_and(|ext| ext == "rlib"))
    })
}

fn judge_wasm32(installed: bool) -> Check {
    match installed {
        true => Check::ok("wasm32", "installed"),
        false => Check::failed(
            "wasm32",
            "not installed",
            format!("rustup target add {WASM_TRIPLE}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::ui::caps::Caps;

    #[test]
    fn test_judges() {
        assert_eq!(
            judge_python(Some("Python 3.12.6".into()), "python3").detail,
            "3.12.6  python3"
        );
        assert_eq!(judge_python(None, "python3").status, Status::Failed);
        assert_eq!(
            version_number("cargo 1.98.0 (797e8a9bc 2026-08-05)"),
            "1.98.0"
        );
        assert_eq!(version_number("odd"), "odd");
        assert_eq!(judge_metor_config(Some(ABI_VERSION)).status, Status::Ok);
        let old = judge_metor_config(Some(ABI_VERSION + 1));
        assert_eq!(old.status, Status::Failed);
        assert!(old.detail.contains("this metor speaks"));
        assert_eq!(judge_metor_config(None).status, Status::Failed);
        assert_eq!(judge_wasm32(true).status, Status::Ok);
        assert_eq!(
            judge_wasm32(false).fix.as_deref(),
            Some("rustup target add wasm32-unknown-unknown")
        );
    }

    #[test]
    fn test_has_rlib() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().display().to_string();
        assert!(!has_rlib(&path));
        std::fs::write(dir.path().join("libcore.rlib"), "").expect("writes");
        assert!(has_rlib(&path));
        assert!(!has_rlib("/no/such/dir"));
    }

    #[test]
    fn test_render_puts_each_fix_under_its_check() {
        let checks = [
            Check::ok("python", "3.12.6  python3"),
            judge_wasm32(false),
            Check {
                name: "uv",
                status: Status::Optional,
                detail: "not found".into(),
                fix: None,
            },
        ];
        let mut out = Vec::new();
        render(&mut out, &checks, &Theme::new(&Caps::plain())).expect("writes to a Vec");
        insta::assert_snapshot!(String::from_utf8(out).expect("utf-8"));
    }
}
