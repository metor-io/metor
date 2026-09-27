//! `pack dev` against the fixture pack: the rendered module and the layout.

mod common;

use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command;

use common::fixture_dir;
use metor_fsw_3::DlPack;
use metor_fsw_3::cli::cargo::{
    PackConfig, WASM_TRIPLE, cargo_build, cdylib_name, triple, wasm_name,
};
use metor_fsw_3::cli::pack_dev::{PackDevError, pack_dev};

const GOLDEN: &str = include_str!("golden/echo_pack.py");

#[test]
fn test_golden_module_type_checks() {
    let Some(pyright) = pyright() else {
        println!("skipped: neither `pyright` nor `uvx` is on PATH");
        return;
    };
    let dir = tempfile::tempdir().expect("a temp dir");
    std::fs::write(dir.path().join("echo_pack.py"), GOLDEN).expect("writes");
    let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("python/metor-config");
    std::fs::write(
        dir.path().join("pyrightconfig.json"),
        format!(
            "{{\"extraPaths\": [\"{}\"], \"typeCheckingMode\": \"standard\"}}",
            config.display()
        ),
    )
    .expect("writes");

    let output = Command::new(&pyright[0])
        .args(&pyright[1..])
        .arg("echo_pack.py")
        .current_dir(dir.path())
        .output()
        .expect("pyright runs");
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The command that runs pyright, absent when nothing can.
fn pyright() -> Option<Vec<String>> {
    for command in [vec!["pyright"], vec!["uvx", "pyright"]] {
        let Ok(status) = Command::new(command[0]).arg("--version").output() else {
            continue;
        };
        if status.status.success() {
            return Some(command.iter().map(|part| (*part).to_string()).collect());
        }
    }
    None
}

#[test]
fn test_pack_dev_installs_module_and_library() {
    let root = fixture_dir();
    let config = PackConfig::read(&root).expect("the fixture names its pack");
    assert_eq!(config.id, "echo");
    assert_eq!(
        (config.krate.as_str(), config.lib.as_str()),
        ("echo-pack", "echo_pack")
    );

    run_pack_dev(&root);
    let module = root.join(".metor").join(&config.module);
    let dylib = module
        .join("_libs")
        .join(triple())
        .join(cdylib_name(&config.lib));
    for path in [
        &module.join("__init__.py"),
        &module.join("py.typed"),
        &dylib,
    ] {
        assert!(path.is_file(), "{} is missing", path.display());
    }
    assert_eq!(
        std::fs::read_to_string(module.join("__init__.py")).expect("reads"),
        GOLDEN
    );
    let inode = dylib.metadata().expect("stats").ino();

    run_pack_dev(&root);
    assert_ne!(
        dylib.metadata().expect("stats").ino(),
        inode,
        "a replaced dylib lands at a new inode"
    );
    // No temp file survives either run.
    let left: Vec<_> = std::fs::read_dir(dylib.parent().expect("a directory"))
        .expect("lists")
        .map(|entry| entry.expect("an entry").file_name())
        .collect();
    assert_eq!(left.len(), 1, "{left:?}");
}

fn run_pack_dev(root: &Path) {
    let output = Command::new(env!("CARGO_BIN_EXE_metor"))
        .args(["pack", "dev"])
        .current_dir(root)
        .output()
        .expect("metor runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_pack_dev_rejects_loaded_destination() {
    let built = cargo_build("echo-pack", &fixture_dir()).expect("fixture builds");
    let temp = tempfile::tempdir().expect("temporary staging root");
    for name in ["pyproject.toml", "Cargo.toml"] {
        std::fs::copy(fixture_dir().join(name), temp.path().join(name)).expect("copy config");
    }
    let config = PackConfig::read(temp.path()).expect("read copied config");
    let module = temp.path().join(".metor").join(&config.module);
    let libs = module.join("_libs").join(triple());
    std::fs::create_dir_all(&libs).expect("create staging directory");
    let dylib = libs.join(cdylib_name(&config.lib));
    std::fs::copy(built, &dylib).expect("stage fixture");
    let init = module.join("__init__.py");
    std::fs::write(&init, "existing module\n").expect("write sentinel module");
    let before = dylib.metadata().expect("library metadata");
    // SAFETY: this is a copy of the fixture built against this workspace's ABI.
    drop(unsafe { DlPack::open(&dylib) }.expect("load staged fixture"));

    assert!(matches!(
        pack_dev(temp.path()),
        Err(PackDevError::AlreadyLoaded(path)) if path == dylib
    ));
    let after = dylib.metadata().expect("library remains present");
    assert_eq!(after.ino(), before.ino());
    assert_eq!(after.len(), before.len());
    assert_eq!(
        after.modified().expect("mtime"),
        before.modified().expect("mtime")
    );
    assert_eq!(
        std::fs::read_to_string(init).expect("module remains"),
        "existing module\n"
    );
    assert!(!module.join("py.typed").exists());
}

/// A pack spelled out so no `Cargo.toml` is read, with `wasm`.
fn wasm_pyproject(krate: &str) -> String {
    let lib = krate.replace('-', "_");
    format!(
        "[project]\nname = \"{krate}\"\n\n[tool.metor.pack]\n\
         id = \"echo\"\ncrate = \"{krate}\"\nlib = \"{lib}\"\nwasm = true\n"
    )
}

/// A pack root under the target dir, where cargo finds this workspace and
/// its config.
fn root_in_target() -> tempfile::TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).expect("a temp dir")
}

#[test]
fn test_wasm_pack_dev_refuses_async() {
    let root = root_in_target();
    let pyproject = wasm_pyproject("echo-pack");
    std::fs::write(root.path().join("pyproject.toml"), pyproject).expect("writes");
    let refused = pack_dev(root.path());
    assert!(
        matches!(&refused, Err(PackDevError::AsyncInWasm { system }) if system == "relay"),
        "{refused:?}"
    );
    let module = root.path().join(".metor/echo_pack");
    assert!(!module.join("__init__.py").exists());
    assert!(!module.join("_libs").join(WASM_TRIPLE).exists());
}

/// The fixture without `relay`, renamed `sync-pack` so its artifacts land
/// beside the fixture's, as a workspace of its own that builds into this
/// workspace's target dir.
fn sync_copy() -> tempfile::TempDir {
    let root = root_in_target();
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let fixture = fixture_dir();
    let manifest = std::fs::read_to_string(fixture.join("Cargo.toml")).expect("reads");
    let manifest = manifest
        .replace("name = \"echo-pack\"", "name = \"sync-pack\"")
        .replace("version.workspace = true", "version = \"0.0.0\"")
        .replace("edition.workspace = true", "edition = \"2024\"")
        .replace("repository.workspace = true\n", "")
        .replace("[lints]\nworkspace = true\n", "")
        .replace(
            "path = \"../../..\"",
            &format!("path = {:?}", env!("CARGO_MANIFEST_DIR")),
        );
    std::fs::write(root.path().join("Cargo.toml"), manifest + "\n[workspace]\n").expect("writes");
    let relay = "    table\n        .register_async(\"relay\", relay(), Default::default)\n        \
                 .expect(\"valid records\");\n";
    let lib = std::fs::read_to_string(fixture.join("src/lib.rs")).expect("reads");
    assert!(lib.contains(relay), "the fixture registers `relay`");
    std::fs::create_dir(root.path().join("src")).expect("creates");
    std::fs::write(root.path().join("src/lib.rs"), lib.replace(relay, "")).expect("writes");
    std::fs::copy(workspace.join("Cargo.lock"), root.path().join("Cargo.lock")).expect("copies");
    let config = root.path().join(".cargo");
    std::fs::create_dir(&config).expect("creates");
    let target = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .parent()
        .expect("the target dir");
    let build = format!("[build]\ntarget-dir = {:?}\n", target.display().to_string());
    std::fs::write(config.join("config.toml"), build).expect("writes");
    let pyproject = wasm_pyproject("sync-pack");
    std::fs::write(root.path().join("pyproject.toml"), pyproject).expect("writes");
    root
}

#[test]
fn test_wasm_pack_dev_stages_module() {
    let root = sync_copy();
    pack_dev(root.path()).expect("pack dev lays out both");
    let module = root.path().join(".metor/sync_pack");
    let staged = module
        .join("_libs")
        .join(WASM_TRIPLE)
        .join(wasm_name("sync_pack"));
    assert!(staged.is_file(), "{} is missing", staged.display());
    let init = std::fs::read_to_string(module.join("__init__.py")).expect("reads");
    assert!(init.contains("    wasm=True,\n)"), "{init}");
}
