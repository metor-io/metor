//! The systems built into the host, which any target can use.
//!
//! Each one is registered under the `fsw.` prefix, which plays the same role
//! a pack's id does for the systems in a pack.

use crate::coordinator::{BuildError, SystemTable};

/// Registers every built-in system into `table`.
pub fn register_builtins(table: &mut SystemTable) -> Result<(), BuildError> {
    table.register(
        "fsw.channel_registry",
        crate::channel::channel_registry(),
        crate::channel::Registry::new,
    )?;
    table.insert(crate::dl::SLOT_TYPE, crate::dl::slot::entry())?;
    #[cfg(unix)]
    {
        table.insert(crate::io::IO_TYPE, crate::io::host::entry())?;
    }
    Ok(())
}
