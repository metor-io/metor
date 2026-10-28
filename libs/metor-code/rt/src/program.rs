//! A compiled program: its descriptor and its function table.

use std::any::Any;
use std::sync::Arc;

use crate::cx::EntryFn;
use crate::desc::{DESC_VERSION, ProgramDesc, TypeDesc};

/// The descriptor symbol a compiled object exports as `mc_program`.
///
/// `desc` is the [`ProgramDesc`] as JSON and `fns` the function table its
/// entries index.
#[repr(C)]
pub struct McProgram {
    pub version: u32,
    pub desc_len: u32,
    pub desc: *const u8,
    pub fns: *const EntryFn,
    pub fns_len: usize,
}

// SAFETY: the symbol is immutable data the compiler emitted.
unsafe impl Sync for McProgram {}

/// An error reading a program.
#[derive(Debug, thiserror::Error)]
pub enum ProgramError {
    #[error("the descriptor is version {0}, but this runtime reads {DESC_VERSION}")]
    Version(u32),
    #[error("the descriptor does not decode: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("type `{ty}` names function {index}, past the table's {len}")]
    Function { ty: String, index: u32, len: usize },
    #[error("type `{ty}` names frame {index}, which the descriptor lacks")]
    Frame { ty: String, index: u32 },
}

/// A program the runtime can step: what it defines and where its code is.
#[derive(Clone)]
pub struct Program {
    pub desc: Arc<ProgramDesc>,
    fns: Arc<[EntryFn]>,
    /// Keeps the memory `fns` points into alive, for a JIT.
    _owner: Option<Arc<dyn Any>>,
}

impl Program {
    /// Checks that every index in `desc` resolves, and wraps it.
    ///
    /// `owner` is dropped with the last clone of the program.
    pub fn new(
        desc: ProgramDesc,
        fns: Vec<EntryFn>,
        owner: Option<Arc<dyn Any>>,
    ) -> Result<Program, ProgramError> {
        if desc.version != DESC_VERSION {
            return Err(ProgramError::Version(desc.version));
        }
        for ty in &desc.types {
            check_type(&desc, ty, fns.len())?;
        }
        Ok(Program {
            desc: Arc::new(desc),
            fns: fns.into(),
            _owner: owner,
        })
    }

    /// Reads the program a compiled object exports.
    ///
    /// # Safety
    /// `raw` must be a descriptor the compiler emitted: `desc` points to
    /// `desc_len` bytes and `fns` to `fns_len` function pointers, all living
    /// for the rest of the process.
    pub unsafe fn from_raw(raw: &McProgram) -> Result<Program, ProgramError> {
        if raw.version != DESC_VERSION {
            return Err(ProgramError::Version(raw.version));
        }
        // SAFETY: the caller's contract.
        let bytes = unsafe { core::slice::from_raw_parts(raw.desc, raw.desc_len as usize) };
        // SAFETY: the caller's contract.
        let fns = unsafe { core::slice::from_raw_parts(raw.fns, raw.fns_len) };
        Program::new(serde_json::from_slice(bytes)?, fns.to_vec(), None)
    }

    /// Returns function `index`, which [`Program::new`] checked.
    pub fn func(&self, index: u32) -> EntryFn {
        self.fns[index as usize]
    }
}

/// Checks that a type's function and frame indices resolve.
fn check_type(desc: &ProgramDesc, ty: &TypeDesc, len: usize) -> Result<(), ProgramError> {
    let entry = &ty.entry;
    let redlines = entry.redlines.iter().map(|redline| redline.func);
    let funcs = [entry.init, entry.step].into_iter().chain(redlines);
    for index in funcs.chain(entry.on_abort) {
        if index as usize >= len {
            return Err(ProgramError::Function {
                ty: ty.name.clone(),
                index,
                len,
            });
        }
    }
    let ports = ty.inputs.iter().chain(&ty.outputs);
    if let Some(port) = ports.into_iter().find(|p| desc.frame_of(p).is_none()) {
        return Err(ProgramError::Frame {
            ty: ty.name.clone(),
            index: port.frame,
        });
    }
    Ok(())
}
