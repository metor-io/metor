//! Ahead-of-time compilation: a `cranelift-object` object file holding the
//! entry points and `mc_program`, linked with the prebuilt mc-rt pack
//! library into a shared library fsw-3's Dylib loader opens unchanged.

use std::path::{Path, PathBuf};
use std::process::Command;

use cranelift_codegen::isa::{self, OwnedTargetIsa};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_module::{DataDescription, DataId, Linkage, Module};
use cranelift_object::{ObjectBuilder, ObjectModule};
use mc_rt::desc::DESC_VERSION;

use crate::check::Imports;
use crate::codegen::{self, CodegenError, Lowered};
use crate::CompileError;

/// The eight symbols every pack exports.
const PACK_SYMBOLS: [&str; 8] = [
    "metor_fsw_abi_version",
    "metor_fsw_pack_def",
    "metor_fsw_def",
    "metor_fsw_create",
    "metor_fsw_execute",
    "metor_fsw_destroy",
    "metor_fsw_alloc",
    "metor_fsw_free",
];

/// An error building a library.
#[derive(Debug, thiserror::Error)]
pub enum AotError {
    #[error(transparent)]
    Compile(#[from] CompileError),
    #[error("target `{0}`: {1}")]
    Target(String, String),
    #[error("writing the object: {0}")]
    Object(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("`{0}` failed with {1}")]
    Tool(String, std::process::ExitStatus),
}

impl From<CodegenError> for AotError {
    fn from(error: CodegenError) -> Self {
        AotError::Compile(error.into())
    }
}

/// What a link needs: the target, the runtime library and the linker.
pub struct LinkOptions<'a> {
    /// The target triple, or `None` for this host.
    pub triple: Option<&'a str>,
    pub runtime: &'a Path,
    /// The linker driver, such as `cc`.
    pub linker: &'a str,
}

/// Compiles `src` to an object file's bytes.
pub fn object(src: &str, imports: &dyn Imports, triple: Option<&str>) -> Result<Vec<u8>, AotError> {
    let prog = crate::front(src, imports)?;
    let isa = target_isa(triple)?;
    let builder = ObjectBuilder::new(isa, "mc_program", codegen::libcall_names()).map_err(CodegenError::from)?;
    let mut module = ObjectModule::new(builder);
    let lowered = codegen::lower(&mut module, &prog)?;
    descriptor(&mut module, &lowered)?;
    module.finish().emit().map_err(|e| AotError::Object(e.to_string()))
}

/// Compiles `src` and links it with the runtime into the shared library
/// `out`.
pub fn build(src: &str, imports: &dyn Imports, options: &LinkOptions<'_>, out: &Path) -> Result<(), AotError> {
    let bytes = object(src, imports, options.triple)?;
    let object = out.with_extension("o");
    std::fs::write(&object, bytes)?;
    let linked = link(&object, options, out);
    let _ = std::fs::remove_file(&object);
    linked
}

/// Links an object with the runtime library.
pub fn link(object: &Path, options: &LinkOptions<'_>, out: &Path) -> Result<(), AotError> {
    let macos = is_macos(options.triple);
    let mut cmd = Command::new(options.linker);
    cmd.arg(if macos { "-dynamiclib" } else { "-shared" });
    cmd.arg("-o").arg(out).arg(object).arg(options.runtime);
    let prefix = if macos { "_" } else { "" };
    for symbol in PACK_SYMBOLS {
        cmd.arg(format!("-Wl,-u,{prefix}{symbol}"));
    }
    let libs: &[&str] = match macos {
        true => &["-liconv", "-lc", "-lm"],
        false => &["-lgcc_s", "-lutil", "-lrt", "-lpthread", "-lm", "-ldl", "-lc"],
    };
    cmd.args(libs);
    run(cmd, options.linker)?;
    if macos && options.triple.is_none() {
        let mut sign = Command::new("codesign");
        sign.args(["-s", "-", "-f"]).arg(out).stderr(std::process::Stdio::null());
        run(sign, "codesign")?;
    }
    Ok(())
}

fn run(mut cmd: Command, name: &str) -> Result<(), AotError> {
    let status = cmd.status()?;
    match status.success() {
        true => Ok(()),
        false => Err(AotError::Tool(name.to_string(), status)),
    }
}

fn is_macos(triple: Option<&str>) -> bool {
    match triple {
        Some(triple) => triple.contains("apple") || triple.contains("darwin"),
        None => cfg!(target_os = "macos"),
    }
}

/// The ISA for `triple`, or for this host, built position-independent with
/// speed optimisations.
pub fn target_isa(triple: Option<&str>) -> Result<OwnedTargetIsa, AotError> {
    let mut flags = settings::builder();
    let target = |e: String| AotError::Target(triple.unwrap_or("host").to_string(), e);
    flags.set("opt_level", "speed").map_err(|e| target(e.to_string()))?;
    flags.set("is_pic", "true").map_err(|e| target(e.to_string()))?;
    let builder = match triple {
        Some(triple) => {
            let parsed = triple.parse().map_err(|e| target(format!("{e}")))?;
            isa::lookup(parsed).map_err(|e| target(e.to_string()))?
        }
        None => cranelift_native::builder().map_err(|e| target(e.to_string()))?,
    };
    builder
        .finish(settings::Flags::new(flags))
        .map_err(|e| target(e.to_string()))
}

/// Defines `mc_program`: the descriptor's JSON, the function table, and the
/// header mc-rt reads them through.
fn descriptor(module: &mut ObjectModule, lowered: &Lowered) -> Result<(), AotError> {
    let ptr = module.target_config().pointer_bytes() as usize;
    if ptr != 8 {
        return Err(AotError::Target("".into(), "only 64-bit targets are supported".into()));
    }
    let json = serde_json::to_vec(&lowered.desc).map_err(|e| AotError::Object(e.to_string()))?;
    let desc = data(module, "mc_desc", json.clone().into_boxed_slice(), Linkage::Local, |_, _| {})?;
    let table = lowered.table.clone();
    let fns = data(module, "mc_fns", vec![0; ptr * table.len()].into_boxed_slice(), Linkage::Local, |m, d| {
        for (index, id) in table.iter().enumerate() {
            let func = m.declare_func_in_data(*id, d);
            d.write_function_addr((index * ptr) as u32, func);
        }
    })?;
    let mut header = Vec::with_capacity(32);
    header.extend_from_slice(&DESC_VERSION.to_le_bytes());
    header.extend_from_slice(&(json.len() as u32).to_le_bytes());
    header.extend_from_slice(&[0; 16]);
    header.extend_from_slice(&(table.len() as u64).to_le_bytes());
    data(module, "mc_program", header.into_boxed_slice(), Linkage::Export, |m, d| {
        let desc = m.declare_data_in_data(desc, d);
        d.write_data_addr(8, desc, 0);
        let fns = m.declare_data_in_data(fns, d);
        d.write_data_addr(16, fns, 0);
    })?;
    Ok(())
}

/// Defines one 8-aligned data object, letting `relocate` add relocations.
fn data(
    module: &mut ObjectModule,
    name: &str,
    bytes: Box<[u8]>,
    linkage: Linkage,
    relocate: impl FnOnce(&mut ObjectModule, &mut DataDescription),
) -> Result<DataId, AotError> {
    let id = module.declare_data(name, linkage, false, false).map_err(CodegenError::from)?;
    let mut desc = DataDescription::new();
    desc.define(bytes);
    desc.set_align(8);
    relocate(module, &mut desc);
    module.define_data(id, &desc).map_err(CodegenError::from)?;
    Ok(id)
}

/// Where a compiled pack's library goes: `<dir>/lib<name>.<dylib|so>`.
pub fn library_path(dir: &Path, name: &str, triple: Option<&str>) -> PathBuf {
    let ext = if is_macos(triple) { "dylib" } else { "so" };
    dir.join(format!("lib{name}.{ext}"))
}
