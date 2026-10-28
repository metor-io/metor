//! The pack exports of a compiled metor-code library.
//!
//! `metor-code build` links this static library with the object it emits.
//! The object defines `mc_program`; this library reads it into a
//! [`SystemTable`] and exports the table through the fsw-3 ABI, so the
//! linked library loads like any pack.

use mc_rt::{McProgram, Program};
use metor_fsw_3::{SystemTable, export_pack};

unsafe extern "C" {
    /// The descriptor the compiled object defines.
    static mc_program: McProgram;
}

/// Builds the pack's table from the compiled descriptor.
///
/// A descriptor that does not read or register leaves the table empty and
/// logs why.
fn build() -> SystemTable {
    // SAFETY: the linked object is compiler output, which defines
    // `mc_program` over data and code that live as long as the library.
    let program = unsafe { Program::from_raw(&mc_program) };
    let table = program
        .map_err(|e| e.to_string())
        .and_then(|program| mc_rt::table(&program).map_err(|e| e.to_string()));
    table.unwrap_or_else(|message| {
        metor_fsw_3::tracing::error!("metor-code program: {message}");
        SystemTable::new()
    })
}

export_pack!(build);
