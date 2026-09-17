//! `pack dev` against the fixture pack: the rendered module and the layout.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use metor_fsw_3::cli::build::{PackConfig, cargo_build, cdylib_name, triple};
use metor_fsw_3::cli::config::PackRef;
use metor_fsw_3::cli::module::render;
use metor_fsw_3::cli::pack_dev::{PackDevError, pack_dev};
use metor_fsw_3::{ABI_VERSION, Pack};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/echo-pack")
}

const GOLDEN: &str = include_str!("golden/echo_pack.py");

#[test]
fn the_fixtures_module_is_the_golden() {
    let built = cargo_build("echo-pack", &root(), false).expect("the fixture builds");
    // SAFETY: the fixture is a metor-fsw-3 pack this workspace just built.
    let pack = unsafe { Pack::open(&built) }.expect("it opens");
    let reference = PackRef {
        id: "echo".into(),
        lib: "echo_pack".into(),
        libs: "unused".into(),
    };
    let text = render(&reference, ABI_VERSION, pack.def()).expect("renders");
    assert_eq!(text, GOLDEN);
    assert!(
        !text.contains(env!("CARGO_MANIFEST_DIR")),
        "no absolute paths"
    );
}

#[test]
fn the_golden_module_type_checks() {
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
fn pack_dev_lays_out_the_module_and_replaces_the_dylib() {
    let root = root();
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
fn pack_dev_rejects_a_loaded_destination_without_changing_files() {
    let built = cargo_build("echo-pack", &root(), false).expect("fixture builds");
    let temp = tempfile::tempdir().expect("temporary staging root");
    for name in ["pyproject.toml", "Cargo.toml"] {
        std::fs::copy(root().join(name), temp.path().join(name)).expect("copy config");
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
    drop(unsafe { Pack::open(&dylib) }.expect("load staged fixture"));

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
