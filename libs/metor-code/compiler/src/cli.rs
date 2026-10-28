//! The `metor-code` command line, which `metor code` runs.

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use metor_fsw_3::ABI_VERSION;
use metor_fsw_3::dl::store::write_atomic;
use metor_fsw_3::dl::{PackRef, cdylib_name, triple};

use crate::aot::{self, AotError, LinkOptions};
use crate::packs::PackDirs;

#[derive(Parser)]
#[command(
    name = "metor code",
    about = "Compile metor-code into metor-fsw-3 packs"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Checks a file and reports its errors.
    Check(Source),
    /// Compiles a file into a pack library and its Python module.
    Build(Build),
}

#[derive(clap::Args)]
pub struct Source {
    /// The `.mc` file.
    pub file: PathBuf,
    /// More directories of built pack modules, for `use frames from`.
    #[arg(long = "pack-dir")]
    pub pack_dirs: Vec<PathBuf>,
}

#[derive(clap::Args)]
pub struct Build {
    #[command(flatten)]
    pub source: Source,
    /// The pack's name, which is also its Python module. Defaults to the
    /// file's stem.
    #[arg(long)]
    pub name: Option<String>,
    /// Where the module goes. Defaults to `.metor` next to the file.
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// The target triple. Defaults to this host.
    #[arg(long)]
    pub target: Option<String>,
    /// The mc-rt pack library to link. Defaults to `$METOR_CODE_RT`, then
    /// one built from this checkout.
    #[arg(long)]
    pub runtime: Option<PathBuf>,
    /// The linker driver.
    #[arg(long, default_value = "cc")]
    pub linker: String,
}

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("reading {0}: {1}")]
    Read(PathBuf, std::io::Error),
    #[error("{0}")]
    Source(String),
    #[error(transparent)]
    Aot(#[from] AotError),
    #[error(transparent)]
    Runtime(#[from] crate::runtime::RuntimeError),
    #[error(transparent)]
    Pack(#[from] metor_fsw_3::PackError),
    #[error(transparent)]
    PackDev(#[from] metor_fsw_3::cli::pack_dev::PackDevError),
    #[error("writing {0}: {1}")]
    Write(PathBuf, std::io::Error),
}

/// Runs the command line, printing any error and exiting 1 on failure.
pub fn main() {
    if let Err(error) = run(Cli::parse()) {
        eprintln!("metor code: {error}");
        std::process::exit(1);
    }
}

pub fn run(cli: Cli) -> Result<(), CliError> {
    match cli.command {
        Command::Check(source) => {
            let (src, dirs) = read(&source)?;
            crate::front(&src, &dirs).map_err(|e| source_error(&source.file, &src, e))?;
            eprintln!("{}: ok", source.file.display());
        }
        Command::Build(args) => {
            for path in build(&args)? {
                eprintln!("{}", path.display());
            }
        }
    }
    Ok(())
}

/// Reads the file and finds the packs it can import from, building the dev
/// packs its `pyproject.toml` lists first.
fn read(source: &Source) -> Result<(String, PackDirs), CliError> {
    let src = std::fs::read_to_string(&source.file)
        .map_err(|e| CliError::Read(source.file.clone(), e))?;
    if src.contains("use frames from") {
        for root in PackDirs::dev_packs(&source.file) {
            metor_fsw_3::cli::pack_dev::pack_dev(&root)?;
        }
    }
    Ok((
        src,
        PackDirs::around(&source.file, source.pack_dirs.clone()),
    ))
}

fn source_error(file: &Path, src: &str, error: crate::CompileError) -> CliError {
    CliError::Source(error.render(&file.display().to_string(), src))
}

/// Builds the library and writes the module. Returns the paths written.
pub fn build(args: &Build) -> Result<Vec<PathBuf>, CliError> {
    let file = &args.source.file;
    let (src, dirs) = read(&args.source)?;
    let name = args.name.clone().unwrap_or_else(|| stem(file));
    let out = args.out.clone().unwrap_or_else(|| module_root(file));
    let module = out.join(&name);
    let host = triple();
    let libs = module
        .join("_libs")
        .join(args.target.as_deref().unwrap_or(&host));
    std::fs::create_dir_all(&libs).map_err(|e| CliError::Write(libs.clone(), e))?;
    let runtime = match &args.runtime {
        Some(path) => path.clone(),
        None => crate::runtime::runtime_lib(args.target.as_deref())?,
    };
    let library = libs.join(cdylib_name(&name));
    let options = LinkOptions {
        triple: args.target.as_deref(),
        runtime: &runtime,
        linker: &args.linker,
    };
    aot::build(&src, &dirs, &options, &library).map_err(|e| match e {
        AotError::Compile(e) => source_error(file, &src, e),
        other => other.into(),
    })?;
    let mut written = vec![library];
    if args.target.is_some() {
        return Ok(written);
    }
    written.extend(write_module(&name, &module)?);
    Ok(written)
}

/// Opens the library just built and writes its typed Python module.
fn write_module(name: &str, module: &Path) -> Result<Vec<PathBuf>, CliError> {
    let reference = PackRef::dylib(name.to_string(), name.to_string(), module.join("_libs"));
    let pack = reference.open()?;
    let text = metor_fsw_3::cli::module::render(&reference, ABI_VERSION, pack.descriptor())?;
    let init = module.join("__init__.py");
    write_atomic(&init, text.as_bytes()).map_err(|e| CliError::Write(init.clone(), e))?;
    let typed = module.join("py.typed");
    write_atomic(&typed, b"").map_err(|e| CliError::Write(typed.clone(), e))?;
    Ok(vec![init, typed])
}

fn stem(file: &Path) -> String {
    file.file_stem()
        .map_or("program".into(), |s| s.to_string_lossy().replace('-', "_"))
}

/// `.metor` next to the file.
fn module_root(file: &Path) -> PathBuf {
    let dir = file
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    dir.join(".metor")
}
